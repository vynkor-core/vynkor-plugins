//! Async front of the plugin: everything that may call other plugins
//! (hybrid `chat_search`, `chat_recall`) lives here; the rest is handed to
//! the synchronous [`request::handle`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::db::{with_db, Db};
use crate::embed::{clip, SESSIONS, TURNS};
use crate::request;
use crate::rpc::Rpc;
use crate::search::{self, SearchParams};

const RRF_K: f64 = 60.0;
const POOL: i64 = 50;
const MODEL_TTL: Duration = Duration::from_secs(60);

pub struct Service {
    db: Db,
    rpc: Rpc,
    cfg: Arc<Config>,
    model: Mutex<Option<(Instant, String)>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallParams {
    vector: Vec<f32>,
    model: String,
    #[serde(default)]
    exclude_session_id: Option<String>,
    #[serde(default)]
    top_k: Option<i64>,
    #[serde(default)]
    min_score: Option<f64>,
}

/// One live vector hit: (session, turn, at, fallback snippet).
type VecHit = (String, Option<String>, i64, String);

fn db_err(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

impl Service {
    pub fn new(db: Db, rpc: Rpc, cfg: Arc<Config>) -> Self {
        Service {
            db,
            rpc,
            cfg,
            model: Mutex::new(None),
        }
    }

    pub async fn handle(&self, action: &str, params: &[u8], now: i64) -> Result<Value, String> {
        match action {
            "chat_search" if self.cfg.semantic => {
                self.hybrid_search(request::parse(action, params)?).await
            }
            "chat_recall" => {
                // validate first: a malformed call is an error with or without
                // the semantic layer
                let p: RecallParams = request::parse(action, params)?;
                if !self.cfg.semantic {
                    return Ok(json!({"hits": []}));
                }
                self.recall(p).await
            }
            _ => {
                let (action, params, cfg) =
                    (action.to_string(), params.to_vec(), Arc::clone(&self.cfg));
                with_db(&self.db, move |c| {
                    request::handle(c, &cfg, &action, &params, now)
                })
                .await?
            }
        }
    }

    async fn hybrid_search(&self, p: SearchParams) -> Result<Value, String> {
        let limit = p.limit.unwrap_or(20).clamp(1, 100) as usize;
        if search::fts_query(&p.query).is_none() {
            return Ok(json!({"hits": []}));
        }
        let fts_p = SearchParams {
            query: p.query.clone(),
            from: p.from,
            to: p.to,
            kind: p.kind.clone(),
            limit: Some(POOL),
        };
        let (fts, vec) = tokio::join!(
            with_db(&self.db, move |c| search::search(c, &fts_p)),
            self.vector_lists(&p)
        );
        let fts = fts??;
        let fts_hits = fts["hits"].as_array().cloned().unwrap_or_default();

        struct Acc {
            score: f64,
            at: i64,
            snippet: String,
            from_fts: bool,
        }
        let mut acc: HashMap<(String, Option<String>), Acc> = HashMap::new();
        let mut bump =
            |key: (String, Option<String>), rank: usize, at: i64, snippet: String, fts: bool| {
                let e = acc.entry(key).or_insert(Acc {
                    score: 0.0,
                    at,
                    snippet: String::new(),
                    from_fts: false,
                });
                e.score += 1.0 / (RRF_K + rank as f64);
                if fts && !e.from_fts {
                    e.snippet = snippet;
                    e.from_fts = true;
                } else if e.snippet.is_empty() {
                    e.snippet = snippet;
                }
            };
        for (i, h) in fts_hits.iter().enumerate() {
            let key = (
                h["session_id"].as_str().unwrap_or("").to_string(),
                h["turn_id"].as_str().map(String::from),
            );
            bump(
                key,
                i + 1,
                h["at"].as_i64().unwrap_or(0),
                h["snippet"].as_str().unwrap_or("").to_string(),
                true,
            );
        }
        for list in vec {
            for (i, (sid, tid, at, text)) in list.into_iter().enumerate() {
                bump((sid, tid), i + 1, at, clip(&text, 160), false);
            }
        }
        let mut rows: Vec<_> = acc.into_iter().collect();
        rows.sort_by(|a, b| {
            b.1.score
                .partial_cmp(&a.1.score)
                .unwrap()
                .then(b.1.at.cmp(&a.1.at))
                .then(a.0.cmp(&b.0))
        });
        let hits: Vec<Value> = rows
            .into_iter()
            .take(limit)
            .map(|((sid, tid), a)| {
                json!({"session_id": sid, "turn_id": tid, "at": a.at, "snippet": a.snippet, "score": a.score})
            })
            .collect();
        Ok(json!({"hits": hits}))
    }

    /// Vector candidates (turns, session summaries), already reduced to live
    /// sessions inside the filters. Any failure or timeout yields an empty list.
    async fn vector_lists(&self, p: &SearchParams) -> Vec<Vec<VecHit>> {
        let mut filter = serde_json::Map::new();
        if let Some(k) = &p.kind {
            filter.insert("kind".into(), json!(k));
        }
        // One embedding for both collections, all inside the vector budget.
        // If embedding is unavailable the vector store embeds the text itself.
        let started = Instant::now();
        let budget = Duration::from_millis(self.cfg.search_vec_timeout_ms as u64);
        let vector: Option<Vec<f32>> = self
            .rpc
            .call(
                "vec_embed",
                json!({"texts": [clip(&p.query, 2000)]}),
                self.cfg.search_vec_timeout_ms,
            )
            .await
            .ok()
            .filter(|v| v["model"] != "fake")
            .and_then(|v| {
                v["vectors"][0]
                    .as_array()
                    .map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect())
            });
        let remaining = budget.saturating_sub(started.elapsed()).as_millis().max(1) as u32;
        let q = |coll: &str| {
            let mut params = json!({"collection": coll, "top_k": POOL});
            match &vector {
                Some(v) => params["vector"] = json!(v),
                None => params["text"] = json!(p.query),
            }
            if !filter.is_empty() {
                params["filter"] = Value::Object(filter.clone());
            }
            self.rpc.call("vec_query", params, remaining)
        };
        let (t, s) = tokio::join!(q(TURNS), q(SESSIONS));
        let (from, to, kind) = (p.from, p.to, p.kind.clone());
        let mut raw: Vec<(bool, Vec<(String, String)>)> = Vec::new();
        for (is_turn, r) in [(true, t), (false, s)] {
            let items = match r {
                Ok(v) => v["results"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|h| {
                                (
                                    h["id"].as_str().unwrap_or("").to_string(),
                                    h["text"].as_str().unwrap_or("").to_string(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            raw.push((is_turn, items));
        }
        with_db(&self.db, move |c| {
            raw.into_iter()
                .map(|(is_turn, items)| {
                    items
                        .into_iter()
                        .filter_map(|(id, text)| {
                            live_hit(c, is_turn, &id, text, from, to, kind.as_deref())
                                .ok()
                                .flatten()
                        })
                        .collect()
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    /// Model of the `chats-sessions` collection (`Some("")` = empty collection),
    /// cached for [`MODEL_TTL`]; `None` if `vector-db` could not say.
    async fn recall_model(&self) -> Option<String> {
        if let Some((at, m)) = &*self.model.lock().unwrap() {
            if at.elapsed() < MODEL_TTL {
                return Some(m.clone());
            }
        }
        let v = self
            .rpc
            .call(
                "vec_stats",
                json!({"collection": SESSIONS}),
                self.cfg.search_vec_timeout_ms,
            )
            .await
            .ok()?;
        let m = v["model"].as_str().unwrap_or("").to_string();
        *self.model.lock().unwrap() = Some((Instant::now(), m.clone()));
        Some(m)
    }

    async fn recall(&self, p: RecallParams) -> Result<Value, String> {
        let empty = Ok(json!({"hits": []}));
        if p.vector.is_empty() {
            return Err("chat_recall: invalid params: vector must not be empty".into());
        }
        if p.model == "fake" || p.model.is_empty() {
            return empty;
        }
        match self.recall_model().await {
            Some(m) if !m.is_empty() && m == p.model => {}
            _ => return empty,
        }
        let top_k = p.top_k.unwrap_or(3).clamp(1, 10);
        let min = p.min_score.unwrap_or(self.cfg.recall_min_score);
        let r = self
            .rpc
            .call(
                "vec_query",
                json!({"collection": SESSIONS, "vector": p.vector, "top_k": top_k + 1}),
                self.cfg.search_vec_timeout_ms.max(2000),
            )
            .await;
        let Ok(r) = r else {
            // the collection may have been re-embedded or vector-db restarted
            *self.model.lock().unwrap() = None;
            return empty;
        };
        let cands: Vec<(String, f64)> = r["results"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|h| Some((h["id"].as_str()?.to_string(), h["score"].as_f64()?)))
                    .filter(|(id, s)| *s >= min && Some(id) != p.exclude_session_id.as_ref())
                    .collect()
            })
            .unwrap_or_default();
        let hits = with_db(&self.db, move |c| {
            let mut out = Vec::new();
            for (id, score) in cands {
                let row: Option<(String, String, i64)> = c
                    .query_row(
                        "SELECT title, summary, last_at FROM sessions WHERE id = ? AND state = 'live'",
                        [&id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(db_err)?;
                if let Some((title, summary, at)) = row {
                    out.push(json!({"session_id": id, "title": title, "summary": summary, "at": at, "score": score}));
                }
            }
            out.truncate(top_k as usize);
            Ok::<_, String>(out)
        })
        .await??;
        Ok(json!({"hits": hits}))
    }
}

/// Keep a vector hit only if its session is live and inside the filters.
fn live_hit(
    c: &Connection,
    is_turn: bool,
    id: &str,
    text: String,
    from: Option<i64>,
    to: Option<i64>,
    kind: Option<&str>,
) -> rusqlite::Result<Option<VecHit>> {
    let row: Option<(String, i64, String)> = if is_turn {
        c.query_row(
            "SELECT t.session_id, t.at, s.kind FROM turns t JOIN sessions s ON s.id = t.session_id
             WHERE t.id = ? AND s.state = 'live'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
    } else {
        c.query_row(
            "SELECT id, last_at, kind FROM sessions WHERE id = ? AND state = 'live'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
    };
    let Some((sid, at, k)) = row else {
        return Ok(None);
    };
    if from.is_some_and(|f| at < f) || to.is_some_and(|t| at > t) || kind.is_some_and(|x| x != k) {
        return Ok(None);
    }
    Ok(Some((sid, is_turn.then(|| id.to_string()), at, text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Fixture, EMBED_MODEL, T0};

    fn svc(f: &Fixture) -> Service {
        Service::new(Arc::clone(&f.db), f.rpc.clone(), Arc::clone(&f.cfg))
    }

    async fn search(s: &Service, q: &str) -> Vec<Value> {
        let p = serde_json::to_vec(&json!({"query": q})).unwrap();
        s.handle("chat_search", &p, T0).await.unwrap()["hits"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[tokio::test]
    async fn hybrid_merges_fts_and_vectors_by_rrf() {
        let f = Fixture::new();
        f.fake.synonym("forecast", "weather");
        let s1 = f.session("lap", "chat", T0);
        let a = f.turn(&s1, "weather today", "sunny", T0);
        let s2 = f.session("lap", "voice", T0);
        let b = f.turn(&s2, "forecast", "rain", T0);
        let s3 = f.session("pc", "chat", T0);
        f.turn(&s3, "pasta recipe", "boil water", T0);
        f.worker.run_once(T0 + 1).await;
        let hits = search(&svc(&f), "weather").await;
        let ids: Vec<&str> = hits.iter().filter_map(|h| h["turn_id"].as_str()).collect();
        assert_eq!(ids[0], a, "found by both lists: first");
        assert!(ids.contains(&b.as_str()), "found only by vector: {hits:?}");
        assert!(!ids.iter().any(|i| !(*i == a || *i == b)));
        assert!(hits[0]["score"].as_f64().unwrap() > hits[1]["score"].as_f64().unwrap());
        assert!(
            hits[0]["snippet"].as_str().unwrap().contains('['),
            "fts snippet kept"
        );
        assert!(!hits[1]["snippet"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn session_summary_hit_has_null_turn_id() {
        let f = Fixture::new();
        f.fake.synonym("forecast", "weather");
        let s1 = f.session("lap", "chat", T0);
        f.turn(&s1, "hello", "hi", T0);
        f.lock()
            .execute("UPDATE sessions SET title='Forecast', summary='forecast', summarized_rev=rev WHERE id=?", [&s1])
            .unwrap();
        f.worker.run_once(T0 + 1).await;
        let hits = search(&svc(&f), "weather").await;
        assert_eq!(hits.len(), 1);
        assert!(hits[0]["turn_id"].is_null());
        assert_eq!(hits[0]["session_id"], s1.as_str());
    }

    #[tokio::test]
    async fn vector_timeout_falls_back_to_fts() {
        let f = Fixture::with_config(|c| c.search_vec_timeout_ms = 300);
        let s1 = f.session("lap", "chat", T0);
        let a = f.turn(&s1, "weather today", "sunny", T0);
        f.worker.run_once(T0 + 1).await;
        f.fake.set_delay("vector-db", 5000);
        let t = Instant::now();
        let hits = search(&svc(&f), "weather").await;
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["turn_id"], a.as_str());
        // failing and missing backends degrade the same way
        f.fake.set_delay("vector-db", 0);
        f.fake.set_failing("vector-db", true);
        assert_eq!(search(&svc(&f), "weather").await.len(), 1);
        f.fake.set_failing("vector-db", false);
        f.fake.set_missing("vector-db", true);
        assert_eq!(search(&svc(&f), "weather").await.len(), 1);
    }

    #[tokio::test]
    async fn search_hides_deleted_sessions_from_vector_hits() {
        let f = Fixture::new();
        f.fake.synonym("forecast", "weather");
        let s1 = f.session("lap", "chat", T0);
        f.turn(&s1, "forecast", "rain", T0);
        f.worker.run_once(T0 + 1).await;
        assert_eq!(search(&svc(&f), "weather").await.len(), 1);
        let s2 = f.session("pc", "chat", T0);
        f.turn(&s2, "hello", "hi", T0);
        f.lock()
            .execute(
                "UPDATE sessions SET title='F', summary='forecast', summarized_rev=rev WHERE id=?",
                [&s2],
            )
            .unwrap();
        f.worker.run_once(T0 + 1).await;
        assert_eq!(search(&svc(&f), "weather").await.len(), 2);
        f.delete(&s1).await; // vectors still in the fake until the cascade ran
        f.delete(&s2).await;
        assert_eq!(f.fake.doc_ids("chats-turns").len(), 2);
        assert_eq!(f.fake.doc_ids("chats-sessions").len(), 1);
        assert!(search(&svc(&f), "weather").await.is_empty());
    }

    #[tokio::test]
    async fn semantic_off_search_is_plain_fts() {
        let f = Fixture::with_config(|c| c.semantic = false);
        let s1 = f.session("lap", "chat", T0);
        f.turn(&s1, "weather", "sunny", T0);
        let hits = search(&svc(&f), "weather").await;
        assert_eq!(hits.len(), 1);
        assert_eq!(f.fake.call_count(), 0);
    }

    async fn recall_fixture() -> (Fixture, String, String) {
        let f = Fixture::new();
        let s1 = f.session("lap", "chat", T0);
        f.turn(&s1, "hello", "hi", T0);
        let s2 = f.session("pc", "chat", T0);
        f.turn(&s2, "hello", "hi", T0);
        {
            let c = f.lock();
            c.execute("UPDATE sessions SET title='Weather', summary='weather sunny today', summarized_rev=rev WHERE id=?", [&s1]).unwrap();
            c.execute("UPDATE sessions SET title='Pasta', summary='cooking pasta recipe', summarized_rev=rev WHERE id=?", [&s2]).unwrap();
        }
        f.worker.run_once(T0 + 1).await;
        (f, s1, s2)
    }

    fn recall_params(f: &Fixture, text: &str, extra: Value) -> Vec<u8> {
        let mut p = json!({"vector": f.fake.embed(text), "model": EMBED_MODEL});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        serde_json::to_vec(&p).unwrap()
    }

    #[tokio::test]
    async fn recall_filters_score_and_excludes_current() {
        let (f, s1, s2) = recall_fixture().await;
        let s = svc(&f);
        let r = s
            .handle(
                "chat_recall",
                &recall_params(&f, "weather sunny today pasta", json!({})),
                T0,
            )
            .await
            .unwrap();
        let hits = r["hits"].as_array().unwrap();
        assert_eq!(
            hits.len(),
            1,
            "weaker pasta session is below min_score: {r}"
        );
        assert_eq!(hits[0]["session_id"], s1.as_str());
        assert_eq!(hits[0]["title"], "Weather");
        assert!(hits[0]["score"].as_f64().unwrap() >= 0.72);
        // excluded current session
        let r = s
            .handle(
                "chat_recall",
                &recall_params(&f, "weather sunny today", json!({"exclude_session_id": s1})),
                T0,
            )
            .await
            .unwrap();
        assert!(r["hits"].as_array().unwrap().is_empty());
        // min_score 0 returns the weaker one too, top_k caps
        let r = s
            .handle(
                "chat_recall",
                &recall_params(
                    &f,
                    "weather sunny today",
                    json!({"min_score": 0.0, "top_k": 1}),
                ),
                T0,
            )
            .await
            .unwrap();
        assert_eq!(r["hits"].as_array().unwrap().len(), 1);
        let r = s
            .handle(
                "chat_recall",
                &recall_params(
                    &f,
                    "weather sunny today pasta",
                    json!({"min_score": 0.0, "top_k": 5}),
                ),
                T0,
            )
            .await
            .unwrap();
        let got: Vec<&str> = r["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["session_id"].as_str().unwrap())
            .collect();
        assert!(got.contains(&s1.as_str()) && got.contains(&s2.as_str()));
        // deleted sessions never come back
        f.delete(&s1).await;
        let r = s
            .handle(
                "chat_recall",
                &recall_params(&f, "weather sunny today", json!({})),
                T0,
            )
            .await
            .unwrap();
        assert!(r["hits"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recall_rejects_fake_or_foreign_model() {
        let (f, _s1, _s2) = recall_fixture().await;
        let s = svc(&f);
        let before = f.fake.calls("vec_query").len();
        for model in ["fake", "some-other-model"] {
            let mut p: Value =
                serde_json::from_slice(&recall_params(&f, "weather sunny today", json!({})))
                    .unwrap();
            p["model"] = json!(model);
            let r = s
                .handle("chat_recall", &serde_json::to_vec(&p).unwrap(), T0)
                .await
                .unwrap();
            assert_eq!(r, json!({"hits": []}), "{model}");
        }
        assert_eq!(
            f.fake.calls("vec_query").len(),
            before,
            "no vector query for a foreign vector"
        );
    }

    #[tokio::test]
    async fn recall_without_vector_db_is_empty_not_error() {
        let (f, _a, _b) = recall_fixture().await;
        f.fake.set_missing("vector-db", true);
        let r = svc(&f)
            .handle(
                "chat_recall",
                &recall_params(&f, "weather sunny today", json!({})),
                T0,
            )
            .await
            .unwrap();
        assert_eq!(r, json!({"hits": []}));
    }

    #[tokio::test]
    async fn search_embeds_the_query_once_and_falls_back_to_text() {
        let f = Fixture::new();
        f.fake.synonym("forecast", "weather");
        let s1 = f.session("lap", "chat", T0);
        f.turn(&s1, "forecast", "rain", T0);
        f.worker.run_once(T0 + 1).await;
        let s = svc(&f);
        assert_eq!(search(&s, "weather").await.len(), 1);
        assert_eq!(f.fake.calls("vec_embed").len(), 1);
        let q = f.fake.calls("vec_query");
        assert_eq!(q.len(), 2);
        assert!(q
            .iter()
            .all(|p| p["vector"].is_array() && p.get("text").is_none()));
        // embedding unavailable: the text goes to vec_query instead
        f.fake.set_action_failing("vec_embed", true);
        assert_eq!(search(&s, "weather").await.len(), 1, "still found via text");
        let q = f.fake.calls("vec_query");
        assert_eq!(q.len(), 4);
        assert!(q[2..]
            .iter()
            .all(|p| p["text"] == "weather" && p.get("vector").is_none()));
    }

    #[tokio::test]
    async fn recall_params_are_validated_even_when_semantic_is_off() {
        let f = Fixture::with_config(|c| c.semantic = false);
        let s = svc(&f);
        let e = s
            .handle(
                "chat_recall",
                br#"{"vector":[1.0],"model":"m","bogus":1}"#,
                T0,
            )
            .await
            .unwrap_err();
        assert!(e.contains("invalid params"), "{e}");
        let e = s.handle("chat_recall", b"{}", T0).await.unwrap_err();
        assert!(e.contains("invalid params"), "{e}");
        let ok = s
            .handle("chat_recall", br#"{"vector":[1.0],"model":"m"}"#, T0)
            .await
            .unwrap();
        assert_eq!(ok, json!({"hits": []}));
        assert_eq!(f.fake.call_count(), 0);
    }

    #[tokio::test]
    async fn recall_model_cache_hits_once_and_resets_on_vec_query_error() {
        let (f, _s1, _s2) = recall_fixture().await;
        let s = svc(&f);
        let p = recall_params(&f, "weather sunny today", json!({}));
        for _ in 0..3 {
            s.handle("chat_recall", &p, T0).await.unwrap();
        }
        assert_eq!(f.fake.calls("vec_stats").len(), 1, "model cached");
        f.fake.set_action_failing("vec_query", true);
        let r = s.handle("chat_recall", &p, T0).await.unwrap();
        assert_eq!(r, json!({"hits": []}));
        f.fake.set_action_failing("vec_query", false);
        let r = s.handle("chat_recall", &p, T0).await.unwrap();
        assert_eq!(r["hits"].as_array().unwrap().len(), 1);
        assert_eq!(
            f.fake.calls("vec_stats").len(),
            2,
            "cache dropped after the error"
        );
    }

    #[tokio::test]
    async fn empty_collection_model_is_cached_too() {
        let f = Fixture::new();
        let s = svc(&f);
        let p = serde_json::to_vec(&json!({"vector": [1.0, 0.0], "model": EMBED_MODEL})).unwrap();
        for _ in 0..3 {
            let r = s.handle("chat_recall", &p, T0).await.unwrap();
            assert_eq!(r, json!({"hits": []}));
        }
        assert_eq!(f.fake.calls("vec_stats").len(), 1);
        assert!(f.fake.calls("vec_query").is_empty());
    }

    /// Run with: cargo test --release --manifest-path plugins/chats/Cargo.toml -- --ignored --nocapture hybrid_search_20k
    #[tokio::test]
    #[ignore]
    async fn hybrid_search_20k_turns_fast() {
        const WORDS: [&str; 12] = [
            "weather",
            "tomorrow",
            "погода",
            "завтра",
            "meeting",
            "calendar",
            "music",
            "light",
            "kitchen",
            "reminder",
            "timer",
            "rain",
        ];
        let f = Fixture::new();
        let mut x: u64 = 0x9E3779B97F4A7C15;
        {
            let mut c = f.lock();
            let tx = c.transaction().unwrap();
            for sidx in 0..200 {
                tx.execute(
                    "INSERT INTO sessions(id, kind, device, source, started_at, last_at, turns, rev, embedded_rev) VALUES (?,?,?,?,?,?,?,?,0)",
                    rusqlite::params![format!("s{sidx}"), "chat", "lap", "bench", 0, 0, 100, 1],
                )
                .unwrap();
                for seq in 1..=100 {
                    let mut text = String::new();
                    while text.chars().count() < 200 {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        text.push_str(WORDS[(x % WORDS.len() as u64) as usize]);
                        text.push(' ');
                    }
                    tx.execute(
                        "INSERT INTO turns(id, session_id, seq, at, user_text, answer_text, status, rev, embedded) VALUES (?,?,?,?,?,?,?,?,1)",
                        rusqlite::params![format!("t{sidx}-{seq}"), format!("s{sidx}"), seq, seq, text.chars().take(50).collect::<String>(), text, "ok", 1],
                    )
                    .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        // the in-memory vector store holds the vectors of 2000 of the turns
        for i in 0..2000 {
            let (sid, tid) = (
                format!("s{}", i / 10),
                format!("t{}-{}", i / 10, i % 10 + 1),
            );
            let text = format!("{} {}", WORDS[i % 12], WORDS[(i / 12) % 12]);
            f.fake
                .handle(
                    "vec_upsert_batch",
                    json!({"collection": "chats-turns", "docs": [{"id": tid, "text": text, "metadata": {"session_id": sid}}]}),
                )
                .await
                .unwrap();
        }
        let s = svc(&f);
        let mut times = Vec::new();
        for _ in 0..5 {
            for q in [
                "weather",
                "погода",
                "meeting calendar",
                "rain light",
                "timer",
                "music kitchen",
            ] {
                let t = Instant::now();
                let hits = search(&s, q).await;
                times.push(t.elapsed());
                assert!(!hits.is_empty(), "no hits for {q}");
            }
        }
        times.sort();
        let median = times[times.len() / 2];
        println!(
            "hybrid search over 20000 turns: median {median:?}, max {:?}",
            times[times.len() - 1]
        );
        assert!(median < Duration::from_millis(100), "median {median:?}");
    }
}
