//! Test doubles: a fake kernel answering the outbound calls of `chats`
//! (`chat_completion`, `vec_*`, `goal_delete`, `memory_forget_session`) and a
//! fixture wiring a temp database, the fake and a worker together.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::config::{test_config, Config};
use crate::db::{with_db, Db};
use crate::rpc::{Rpc, RpcCall};
use crate::worker::Worker;
use crate::{schema, store};

pub const EMBED_MODEL: &str = "fake-embed";
const DIM: usize = 16;

#[derive(Clone, Debug)]
pub struct Doc {
    pub id: String,
    pub text: String,
    pub vector: Vec<f32>,
    pub metadata: Value,
}

pub type Hook = Arc<dyn Fn(&Value) + Send + Sync>;

#[derive(Default)]
pub struct St {
    /// Every call that reached the kernel: (action, params).
    pub log: Vec<(String, Value)>,
    /// Scripted `chat_completion` replies (content or error text).
    pub completions: VecDeque<Result<String, String>>,
    pub vectors: BTreeMap<String, Vec<Doc>>,
    /// Plugin names ("ai", "vector-db", "agent") that answer `action not found`.
    pub missing: HashSet<&'static str>,
    /// Plugins that answer with an error.
    pub failing: HashSet<&'static str>,
    pub delay_ms: HashMap<&'static str, u64>,
    pub model: String,
    pub synonyms: HashMap<String, String>,
    pub hooks: HashMap<String, Hook>,
    /// Document ids `vec_upsert_batch` rejects (the whole batch fails).
    pub poison: HashSet<String>,
    /// Single actions that answer with an error.
    pub failing_actions: HashSet<String>,
}

#[derive(Clone)]
pub struct FakeKernel {
    pub st: Arc<Mutex<St>>,
}

pub fn plugin_of(action: &str) -> &'static str {
    match action {
        "chat_completion" | "embedding" => "ai",
        a if a.starts_with("vec_") => "vector-db",
        _ => "agent",
    }
}

fn fnv(s: &str) -> usize {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h as usize
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum()
}

impl FakeKernel {
    pub fn new() -> Self {
        FakeKernel {
            st: Arc::new(Mutex::new(St {
                model: EMBED_MODEL.into(),
                ..Default::default()
            })),
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut St) -> R) -> R {
        f(&mut self.st.lock().unwrap())
    }

    pub fn script_completion(&self, reply: Result<&str, &str>) {
        self.with(|s| {
            s.completions
                .push_back(reply.map(String::from).map_err(String::from))
        });
    }

    pub fn set_missing(&self, plugin: &'static str, on: bool) {
        self.with(|s| {
            if on {
                s.missing.insert(plugin);
            } else {
                s.missing.remove(plugin);
            }
        });
    }

    pub fn set_failing(&self, plugin: &'static str, on: bool) {
        self.with(|s| {
            if on {
                s.failing.insert(plugin);
            } else {
                s.failing.remove(plugin);
            }
        });
    }

    pub fn poison_doc(&self, id: &str) {
        self.with(|s| {
            s.poison.insert(id.to_string());
        });
    }

    pub fn set_action_failing(&self, action: &str, on: bool) {
        self.with(|s| {
            if on {
                s.failing_actions.insert(action.to_string());
            } else {
                s.failing_actions.remove(action);
            }
        });
    }

    pub fn set_delay(&self, plugin: &'static str, ms: u64) {
        self.with(|s| {
            s.delay_ms.insert(plugin, ms);
        });
    }

    pub fn synonym(&self, word: &str, canonical: &str) {
        self.with(|s| {
            s.synonyms.insert(word.to_string(), canonical.to_string());
        });
    }

    /// Run `f` (synchronously) whenever `action` is about to be answered.
    pub fn on_call(&self, action: &str, f: impl Fn(&Value) + Send + Sync + 'static) {
        self.with(|s| {
            s.hooks.insert(action.to_string(), Arc::new(f));
        });
    }

    pub fn calls(&self, action: &str) -> Vec<Value> {
        self.with(|s| {
            s.log
                .iter()
                .filter(|(a, _)| a == action)
                .map(|(_, p)| p.clone())
                .collect()
        })
    }

    pub fn call_count(&self) -> usize {
        self.with(|s| s.log.len())
    }

    pub fn doc_ids(&self, collection: &str) -> Vec<String> {
        self.with(|s| {
            let mut v: Vec<String> = s
                .vectors
                .get(collection)
                .map(|d| d.iter().map(|d| d.id.clone()).collect())
                .unwrap_or_default();
            v.sort();
            v
        })
    }

    /// Bag-of-words embedding with synonym folding, L2-normalized.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        let syn = self.with(|s| s.synonyms.clone());
        let mut v = vec![0f32; DIM];
        for w in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let w = w.to_lowercase();
            let w = syn.get(&w).cloned().unwrap_or(w);
            v[fnv(&w) % DIM] += 1.0;
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            v.iter_mut().for_each(|x| *x /= n);
        }
        v
    }

    /// Answer one call the way the kernel + target plugin would. `Err` is the
    /// bare error text (the caller adds the `"{action} failed: "` prefix).
    pub async fn handle(&self, action: &str, params: Value) -> Result<Value, String> {
        let plugin = plugin_of(action);
        let (missing, failing, delay, hook) = self.with(|s| {
            s.log.push((action.to_string(), params.clone()));
            (
                s.missing.contains(plugin),
                s.failing.contains(plugin),
                s.delay_ms.get(plugin).copied(),
                s.hooks.get(action).cloned(),
            )
        });
        if let Some(ms) = delay {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        if let Some(h) = hook {
            h(&params);
        }
        if missing {
            return Err("action not found".into());
        }
        if failing || self.with(|s| s.failing_actions.contains(action)) {
            return Err(format!("{plugin} backend unavailable"));
        }
        match action {
            "chat_completion" => match self.with(|s| s.completions.pop_front()) {
                Some(Ok(content)) => Ok(json!({"content": content})),
                Some(Err(e)) => Err(e),
                None => Err("no scripted completion".into()),
            },
            "vec_upsert_batch" => {
                let coll = params["collection"].as_str().unwrap_or("").to_string();
                let docs = params["docs"].as_array().cloned().unwrap_or_default();
                let n = docs.len();
                let bad = self.with(|s| {
                    docs.iter()
                        .find(|d| s.poison.contains(d["id"].as_str().unwrap_or("")))
                        .map(|d| d["id"].as_str().unwrap_or("").to_string())
                });
                if let Some(id) = bad {
                    return Err(format!("invalid document {id}"));
                }
                for d in docs {
                    let id = d["id"].as_str().unwrap_or("").to_string();
                    let text = d["text"].as_str().unwrap_or("").to_string();
                    let vector = self.embed(&text);
                    let doc = Doc {
                        id: id.clone(),
                        text,
                        vector,
                        metadata: d["metadata"].clone(),
                    };
                    self.with(|s| {
                        let list = s.vectors.entry(coll.clone()).or_default();
                        list.retain(|x| x.id != id);
                        list.push(doc);
                    });
                }
                Ok(json!({"ok": true, "count": n, "dim": DIM}))
            }
            "vec_embed" => {
                let texts = params["texts"].as_array().cloned().unwrap_or_default();
                let vectors: Vec<Vec<f32>> = texts
                    .iter()
                    .map(|t| self.embed(t.as_str().unwrap_or("")))
                    .collect();
                Ok(json!({"vectors": vectors, "model": self.with(|s| s.model.clone()), "dim": DIM}))
            }
            "vec_query" => {
                let coll = params["collection"].as_str().unwrap_or("");
                let qv: Vec<f32> = match params.get("vector").and_then(Value::as_array) {
                    Some(a) => a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect(),
                    None => self.embed(params["text"].as_str().unwrap_or("")),
                };
                let top_k = params["top_k"].as_u64().unwrap_or(5) as usize;
                let filter = params["filter"].as_object().cloned().unwrap_or_default();
                let docs = self.with(|s| s.vectors.get(coll).cloned().unwrap_or_default());
                let mut scored: Vec<(f64, Doc)> = docs
                    .into_iter()
                    .filter(|d| filter.iter().all(|(k, v)| &d.metadata[k] == v))
                    .map(|d| (cosine(&qv, &d.vector), d))
                    .filter(|(s, _)| *s > 0.0)
                    .collect();
                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(a.1.id.cmp(&b.1.id)));
                scored.truncate(top_k);
                let results: Vec<Value> = scored
                    .into_iter()
                    .map(|(s, d)| {
                        json!({"id": d.id, "score": s, "text": d.text, "metadata": d.metadata})
                    })
                    .collect();
                Ok(json!({"results": results}))
            }
            "vec_delete" => {
                let coll = params["collection"].as_str().unwrap_or("").to_string();
                let id = params["id"].as_str().unwrap_or("").to_string();
                if self.with(|s| s.poison.contains(&id)) {
                    return Err(format!("cannot delete {id}"));
                }
                let deleted = self.with(|s| {
                    let list = s.vectors.entry(coll).or_default();
                    let before = list.len();
                    list.retain(|d| d.id != id);
                    list.len() != before
                });
                Ok(json!({"deleted": deleted}))
            }
            "vec_stats" => {
                let coll = params["collection"].as_str().unwrap_or("");
                let (count, model) = self.with(|s| {
                    let n = s.vectors.get(coll).map(Vec::len).unwrap_or(0);
                    (
                        n,
                        if n > 0 {
                            s.model.clone()
                        } else {
                            String::new()
                        },
                    )
                });
                Ok(json!({"count": count, "dim": if count > 0 { DIM } else { 0 }, "model": model}))
            }
            "goal_delete" => Ok(json!({"deleted": true})),
            "memory_forget_session" => Ok(json!({"deleted": 0})),
            other => Err(format!("fake kernel: unexpected action {other}")),
        }
    }

    /// An [`Rpc`] served directly from the channel (no socket).
    pub fn rpc(&self) -> Rpc {
        let (tx, mut rx) = mpsc::channel::<RpcCall>(64);
        let fake = self.clone();
        tokio::spawn(async move {
            while let Some(call) = rx.recv().await {
                let fake = fake.clone();
                tokio::spawn(async move {
                    let params: Value =
                        serde_json::from_slice(&call.params_json).unwrap_or(Value::Null);
                    let res = fake
                        .handle(&call.action, params)
                        .await
                        .map_err(|e| format!("{} failed: {e}", call.action));
                    let _ = call.reply.send(res);
                });
            }
        });
        Rpc::new(tx)
    }
}

pub struct Fixture {
    pub _dir: tempfile::TempDir,
    pub db: Db,
    pub cfg: Arc<Config>,
    pub fake: FakeKernel,
    pub rpc: Rpc,
    pub worker: Arc<Worker>,
    /// `changed` events the worker emitted (their `action`).
    pub events: Arc<Mutex<Vec<String>>>,
}

pub const T0: i64 = 1_000_000_000_000;

impl Fixture {
    pub fn new() -> Self {
        Self::with_config(|_| {})
    }

    pub fn with_config(tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let conn = schema::open(&dir.path().join("chats.db")).unwrap();
        let db: Db = Arc::new(Mutex::new(conn));
        let mut cfg = test_config();
        tweak(&mut cfg);
        let cfg = Arc::new(cfg);
        let fake = FakeKernel::new();
        let rpc = fake.rpc();
        let events = Arc::new(Mutex::new(Vec::new()));
        let worker = Worker::new(Arc::clone(&db), rpc.clone(), Arc::clone(&cfg), {
            let events = Arc::clone(&events);
            Arc::new(move |a: &str| events.lock().unwrap().push(a.to_string()))
        });
        Fixture {
            _dir: dir,
            db,
            cfg,
            fake,
            rpc,
            worker,
            events,
        }
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.db.lock().unwrap()
    }

    pub fn session(&self, device: &str, kind: &str, now: i64) -> String {
        let mut c = self.lock();
        let r = store::resolve(
            &mut c,
            &self.cfg,
            &store::tests::rp(device, kind, None),
            now,
        )
        .unwrap();
        store::tests::sid(&r)
    }

    pub fn turn(&self, sid: &str, user: &str, answer: &str, now: i64) -> String {
        let mut c = self.lock();
        store::append(&mut c, &self.cfg, store::tests::ap(sid, user, answer), now).unwrap()
            ["turn_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    pub fn turn_with_goal(
        &self,
        sid: &str,
        user: &str,
        answer: &str,
        goal: &str,
        now: i64,
    ) -> String {
        let mut p = store::tests::ap(sid, user, answer);
        p.goal_id = Some(goal.into());
        let mut c = self.lock();
        store::append(&mut c, &self.cfg, p, now).unwrap()["turn_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    pub fn q<T: rusqlite::types::FromSql>(&self, sql: &str, p: impl rusqlite::Params) -> T {
        self.lock().query_row(sql, p, |r| r.get(0)).unwrap()
    }

    /// Multi-column variant of [`Fixture::q`] with a single text parameter.
    pub fn q2<T>(&self, sql: &str, id: &str) -> T
    where
        T: for<'a> TryFrom<&'a rusqlite::Row<'a>, Error = rusqlite::Error>,
    {
        self.lock()
            .query_row(sql, [id], |r| T::try_from(r))
            .unwrap()
    }

    pub async fn delete(&self, sid: &str) {
        let sid = sid.to_string();
        with_db(&self.db, move |c| store::delete(c, &sid).unwrap())
            .await
            .unwrap();
    }
}
