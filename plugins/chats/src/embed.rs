//! Worker embedding pass: turns (tier `full` sessions) into `chats-turns`,
//! session summaries into `chats-sessions`.

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::db::with_db;
use crate::worker::Worker;

pub const TURNS: &str = "chats-turns";
pub const SESSIONS: &str = "chats-sessions";
const BATCH: usize = 64;
const TEXT_MAX_CHARS: usize = 2000;

fn db_err(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

pub fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

struct TurnDoc {
    id: String,
    session_id: String,
    text: String,
    meta: Value,
}

fn pending_turns(conn: &Connection) -> Result<Vec<TurnDoc>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT t.id, t.session_id, t.at, t.user_text, t.answer_text, s.kind
             FROM turns t JOIN sessions s ON s.id = t.session_id
             WHERE t.embedded = 0 AND s.state = 'live' AND s.tier = 'full'
             ORDER BY t.rowid LIMIT ?",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([BATCH as i64], |r| {
            let (id, sid, at, q, a, kind): (String, String, i64, String, String, String) = (
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            );
            Ok(TurnDoc {
                text: clip(&format!("Q: {q}\nA: {a}"), TEXT_MAX_CHARS),
                meta: json!({"session_id": sid, "kind": kind, "at": at}),
                id,
                session_id: sid,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?;
    Ok(rows)
}

/// Mark turns embedded; returns the distinct sessions among `ids` that are
/// no longer live (they must be cleaned from the vector store again).
fn mark_turns(
    conn: &mut Connection,
    docs: &[TurnDoc],
) -> Result<Vec<(String, Vec<String>)>, String> {
    let tx = conn.transaction().map_err(db_err)?;
    for d in docs {
        tx.execute("UPDATE turns SET embedded = 1 WHERE id = ?", [&d.id])
            .map_err(db_err)?;
    }
    let mut dead: Vec<(String, Vec<String>)> = Vec::new();
    for d in docs {
        let live: bool = tx
            .query_row(
                "SELECT count(*) FROM sessions WHERE id = ? AND state = 'live'",
                [&d.session_id],
                |r| r.get::<_, i64>(0),
            )
            .map_err(db_err)?
            > 0;
        if !live {
            match dead.iter_mut().find(|(s, _)| *s == d.session_id) {
                Some((_, ids)) => ids.push(d.id.clone()),
                None => dead.push((d.session_id.clone(), vec![d.id.clone()])),
            }
        }
    }
    tx.commit().map_err(db_err)?;
    Ok(dead)
}

struct SessionDoc {
    id: String,
    text: String,
    meta: Value,
    summarized_rev: i64,
}

fn pending_sessions(conn: &Connection) -> Result<Vec<SessionDoc>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, title, summary, kind, last_at, summarized_rev FROM sessions
             WHERE state = 'live' AND summary != '' AND embedded_rev < summarized_rev
             ORDER BY last_at LIMIT ?",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([BATCH as i64], |r| {
            let (id, title, summary, kind, at, rev): (String, String, String, String, i64, i64) = (
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            );
            Ok(SessionDoc {
                text: clip(&format!("{title}\n{summary}"), TEXT_MAX_CHARS),
                meta: json!({"session_id": id, "kind": kind, "at": at}),
                id,
                summarized_rev: rev,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?;
    Ok(rows)
}

fn mark_sessions(conn: &mut Connection, docs: &[SessionDoc]) -> Result<Vec<String>, String> {
    let tx = conn.transaction().map_err(db_err)?;
    let mut dead = Vec::new();
    for d in docs {
        let n = tx
            .execute(
                "UPDATE sessions SET embedded_rev = MAX(embedded_rev, ?1) WHERE id = ?2 AND state = 'live'",
                params![d.summarized_rev, d.id],
            )
            .map_err(db_err)?;
        if n == 0 {
            dead.push(d.id.clone());
        }
    }
    tx.commit().map_err(db_err)?;
    Ok(dead)
}

impl Worker {
    /// Embed pending turns and session summaries. A missing `vector-db`
    /// leaves everything pending for the next pass.
    pub(crate) async fn embed_pass(&self) {
        let docs = match with_db(&self.db, |c| pending_turns(c)).await {
            Ok(Ok(d)) => d,
            Ok(Err(e)) | Err(e) => return self.warn_once("embed-db", &e),
        };
        if !docs.is_empty() {
            let payload: Vec<Value> = docs
                .iter()
                .map(|d| json!({"id": d.id, "text": d.text, "metadata": d.meta}))
                .collect();
            let r = self
                .rpc
                .call(
                    "vec_upsert_batch",
                    json!({"collection": TURNS, "docs": payload}),
                    60_000,
                )
                .await;
            if let Err(e) = r {
                return self.warn_once("embed-vector-db", &e);
            }
            let dead = with_db(&self.db, move |c| match mark_turns(c, &docs) {
                Ok(dead) => dead
                    .into_iter()
                    .try_for_each(|(sid, ids)| crate::cascade::requeue(c, &sid, &ids)),
                Err(e) => Err(e),
            })
            .await;
            if let Ok(Err(e)) | Err(e) = dead {
                return self.warn_once("embed-db", &e);
            }
        }
        let sdocs = match with_db(&self.db, |c| pending_sessions(c)).await {
            Ok(Ok(d)) => d,
            Ok(Err(e)) | Err(e) => return self.warn_once("embed-db", &e),
        };
        if sdocs.is_empty() {
            return;
        }
        let payload: Vec<Value> = sdocs
            .iter()
            .map(|d| json!({"id": d.id, "text": d.text, "metadata": d.meta}))
            .collect();
        if let Err(e) = self
            .rpc
            .call(
                "vec_upsert_batch",
                json!({"collection": SESSIONS, "docs": payload}),
                60_000,
            )
            .await
        {
            return self.warn_once("embed-vector-db", &e);
        }
        let dead = with_db(&self.db, move |c| {
            let dead = mark_sessions(c, &sdocs)?;
            for sid in dead {
                crate::cascade::requeue(c, &sid, &[])?;
            }
            Ok::<(), String>(())
        })
        .await;
        if let Ok(Err(e)) | Err(e) = dead {
            self.warn_once("embed-db", &e);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::testkit::{Fixture, T0};

    #[tokio::test]
    async fn turns_and_summaries_get_embedded_once() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        let t1 = f.turn(&sid, "what is the weather", "sunny", T0);
        let t2 = f.turn(&sid, "and tomorrow", "rain", T0 + 1);
        f.fake.script_completion(Ok(
            r#"{"title":"Weather","summary":"Weather talk.","tags":["weather"]}"#,
        ));
        let later = T0 + 1_000_000;
        f.worker.run_once(later).await;
        let mut ids = vec![t1, t2];
        ids.sort();
        assert_eq!(f.fake.doc_ids("chats-turns"), ids);
        assert_eq!(f.fake.doc_ids("chats-sessions"), vec![sid.clone()]);
        let upserts = f.fake.calls("vec_upsert_batch");
        assert_eq!(upserts.len(), 2);
        let d = &upserts[0]["docs"][0];
        assert!(d["text"].as_str().unwrap().starts_with("Q: "));
        assert_eq!(d["metadata"]["session_id"], sid.as_str());
        assert_eq!(d["metadata"]["kind"], "chat");
        assert_eq!(upserts[1]["docs"][0]["text"], "Weather\nWeather talk.");
        // nothing new -> no more upserts
        f.worker.run_once(later).await;
        assert_eq!(f.fake.calls("vec_upsert_batch").len(), 2);
        // a new turn is embedded alone
        f.turn(&sid, "thanks", "welcome", T0 + 2);
        f.worker.run_once(T0 + 10).await; // not idle: no summary pass
        let u = f.fake.calls("vec_upsert_batch");
        assert_eq!(u.len(), 3);
        assert_eq!(u[2]["docs"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn light_tier_sessions_get_no_turn_vectors() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        f.turn(&sid, "hi", "hello", T0);
        f.lock()
            .execute("UPDATE sessions SET tier='light' WHERE id=?", [&sid])
            .unwrap();
        f.fake.script_completion(Err("skip"));
        f.worker.run_once(T0 + 1_000_000).await;
        assert!(f.fake.doc_ids("chats-turns").is_empty());
    }

    #[tokio::test]
    async fn missing_vector_db_leaves_turns_pending() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        f.turn(&sid, "hi", "hello", T0);
        f.fake.set_missing("vector-db", true);
        f.worker.run_once(T0).await;
        let n: i64 = f.q("SELECT count(*) FROM turns WHERE embedded=0", []);
        assert_eq!(n, 1);
    }
}
