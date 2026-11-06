//! Worker embedding pass: turns (tier `full` sessions) into `chats-turns`,
//! session summaries into `chats-sessions`.

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::db::with_db;
use crate::rpc::is_missing_plugin;
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

/// Mark turns embedded.
fn set_embedded(conn: &mut Connection, docs: &[TurnDoc]) -> Result<(), String> {
    let tx = conn.transaction().map_err(db_err)?;
    for d in docs {
        tx.execute("UPDATE turns SET embedded = 1 WHERE id = ?", [&d.id])
            .map_err(db_err)?;
    }
    tx.commit().map_err(db_err)
}

/// (session, turn) pairs a vector write was attempted for.
type Attempted = Vec<(String, String)>;

/// Sessions among `attempted` that are no longer live, with their turn ids.
/// Whatever reached vector-db for them (also by a call that timed out or
/// failed half way) must be deleted again.
fn dead_sessions(
    conn: &Connection,
    attempted: &Attempted,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut dead: Vec<(String, Vec<String>)> = Vec::new();
    for (sid, tid) in attempted {
        let live: bool = conn
            .query_row(
                "SELECT count(*) FROM sessions WHERE id = ? AND state = 'live'",
                [sid],
                |r| r.get::<_, i64>(0),
            )
            .map_err(db_err)?
            > 0;
        if !live {
            match dead.iter_mut().find(|(s, _)| s == sid) {
                Some((_, ids)) => ids.push(tid.clone()),
                None => dead.push((sid.clone(), vec![tid.clone()])),
            }
        }
    }
    Ok(dead)
}

/// Sessions among `attempted` that are live but no longer `full` (compacted or
/// summarized while the write was in flight), with their turn ids: the vectors
/// just written must go again.
fn demoted_sessions(
    conn: &Connection,
    attempted: &Attempted,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for (sid, tid) in attempted {
        let demoted: bool = conn
            .query_row(
                "SELECT count(*) FROM sessions WHERE id = ? AND state = 'live' AND tier != 'full'",
                [sid],
                |r| r.get::<_, i64>(0),
            )
            .map_err(db_err)?
            > 0;
        if demoted {
            match out.iter_mut().find(|(s, _)| s == sid) {
                Some((_, ids)) => ids.push(tid.clone()),
                None => out.push((sid.clone(), vec![tid.clone()])),
            }
        }
    }
    Ok(out)
}

/// Queue cleanup for whatever may have reached vector-db for sessions that
/// died or were compacted while the write was running.
fn requeue_stale(conn: &mut Connection, attempted: &Attempted) -> Result<(), String> {
    for (sid, ids) in dead_sessions(conn, attempted)? {
        crate::cascade::requeue(conn, &sid, &ids)?;
    }
    for (sid, ids) in demoted_sessions(conn, attempted)? {
        crate::cascade::enqueue_vectors(conn, &sid, &ids)?;
    }
    Ok(())
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

/// After this many individual failures a turn is given up (`embedded = -1`).
const MAX_DOC_ATTEMPTS: i64 = 3;

fn payload(docs: &[TurnDoc]) -> Vec<Value> {
    docs.iter()
        .map(|d| json!({"id": d.id, "text": d.text, "metadata": d.meta}))
        .collect()
}

/// A document the vector store rejected on its own: count the attempt and
/// stop retrying it after [`MAX_DOC_ATTEMPTS`] so it cannot block the queue.
fn bump_attempts(conn: &mut Connection, docs: &[TurnDoc]) -> Result<(), String> {
    let tx = conn.transaction().map_err(db_err)?;
    for d in docs {
        tx.execute(
            "UPDATE turns SET embed_attempts = embed_attempts + 1,
                 embedded = CASE WHEN embed_attempts + 1 >= ?1 THEN -1 ELSE embedded END
             WHERE id = ?2",
            params![MAX_DOC_ATTEMPTS, d.id],
        )
        .map_err(db_err)?;
    }
    tx.commit().map_err(db_err)
}

impl Worker {
    /// Embed pending turns and session summaries. The two halves are
    /// independent: a failing turn batch never skips the summaries.
    pub(crate) async fn embed_pass(&self) {
        self.embed_turns().await;
        self.embed_sessions().await;
    }

    async fn embed_turns(&self) {
        let docs = match with_db(&self.db, |c| pending_turns(c)).await {
            Ok(Ok(d)) => d,
            Ok(Err(e)) | Err(e) => {
                self.warn("db", &e);
                return;
            }
        };
        if docs.is_empty() {
            return;
        }
        let attempted: Attempted = docs
            .iter()
            .map(|d| (d.session_id.clone(), d.id.clone()))
            .collect();
        let batch = self
            .rpc
            .call(
                "vec_upsert_batch",
                json!({"collection": TURNS, "docs": payload(&docs)}),
                60_000,
            )
            .await;
        let (good, bad) = match batch {
            Ok(_) => {
                self.ok("vector-db");
                (docs, Vec::new())
            }
            Err(e) if is_missing_plugin(&e) => {
                self.warn("vector-db", &e);
                return;
            }
            Err(e) => {
                // One bad document fails the whole batch. If vector-db answers a
                // cheap probe the backend is fine: retry document by document.
                self.warn("vector-db", &e);
                if self
                    .rpc
                    .call("vec_stats", json!({"collection": TURNS}), 5_000)
                    .await
                    .is_err()
                {
                    // Backend trouble. The batch may still have been written
                    // (a timeout says nothing): clean up for dead sessions.
                    let r = with_db(&self.db, move |c| requeue_stale(c, &attempted)).await;
                    if let Ok(Err(e)) | Err(e) = r {
                        self.warn("db", &e);
                    }
                    self.kick_cleanup();
                    return;
                }
                let mut good = Vec::new();
                let mut bad = Vec::new();
                for d in docs {
                    let one = self
                        .rpc
                        .call(
                            "vec_upsert_batch",
                            json!({"collection": TURNS, "docs": payload(std::slice::from_ref(&d))}),
                            30_000,
                        )
                        .await;
                    match one {
                        Ok(_) => good.push(d),
                        Err(e) => {
                            self.warn("vector-db-doc", &format!("document {} rejected: {e}", d.id));
                            bad.push(d);
                        }
                    }
                }
                (good, bad)
            }
        };
        let r = with_db(&self.db, move |c| {
            bump_attempts(c, &bad)?;
            set_embedded(c, &good)?;
            // rejected or timed-out documents count as possibly written too
            requeue_stale(c, &attempted)
        })
        .await;
        if let Ok(Err(e)) | Err(e) = r {
            self.warn("db", &e);
        }
        self.kick_cleanup(); // a session may have died meanwhile
    }

    async fn embed_sessions(&self) {
        let sdocs = match with_db(&self.db, |c| pending_sessions(c)).await {
            Ok(Ok(d)) => d,
            Ok(Err(e)) | Err(e) => {
                self.warn("db", &e);
                return;
            }
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
            self.warn("vector-db", &e);
            return;
        }
        self.ok("vector-db");
        let dead = with_db(&self.db, move |c| {
            let dead = mark_sessions(c, &sdocs)?;
            for sid in dead {
                crate::cascade::requeue(c, &sid, &[])?;
            }
            Ok::<(), String>(())
        })
        .await;
        if let Ok(Err(e)) | Err(e) = dead {
            self.warn("db", &e);
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

    #[tokio::test]
    async fn poison_document_does_not_block_the_queue() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        let a = f.turn(&sid, "first", "ok", T0);
        let bad = f.turn(&sid, "poison", "doc", T0 + 1);
        let c = f.turn(&sid, "third", "ok", T0 + 2);
        f.fake.poison_doc(&bad);
        f.worker.run_work(T0 + 10).await;
        let mut good = vec![a, c.clone()];
        good.sort();
        assert_eq!(
            f.fake.doc_ids("chats-turns"),
            good,
            "good docs stored one by one"
        );
        let (emb, att): (i64, i64) = f.q2(
            "SELECT embedded, embed_attempts FROM turns WHERE id=?",
            &bad,
        );
        assert_eq!((emb, att), (0, 1));
        // newer turns are not held up behind it
        let d = f.turn(&sid, "fourth", "ok", T0 + 3);
        f.worker.run_work(T0 + 10).await;
        assert!(f.fake.doc_ids("chats-turns").contains(&d));
        // after three rejections it is given up
        f.worker.run_work(T0 + 10).await;
        let (emb, att): (i64, i64) = f.q2(
            "SELECT embedded, embed_attempts FROM turns WHERE id=?",
            &bad,
        );
        assert_eq!((emb, att), (-1, 3));
        let calls = f.fake.calls("vec_upsert_batch").len();
        f.worker.run_work(T0 + 10).await;
        assert_eq!(
            f.fake.calls("vec_upsert_batch").len(),
            calls,
            "nothing left to try"
        );
    }

    #[tokio::test]
    async fn backend_outage_does_not_burn_document_attempts() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        let t = f.turn(&sid, "hello", "world", T0);
        f.fake.set_failing("vector-db", true);
        for _ in 0..5 {
            f.worker.run_work(T0 + 10).await;
        }
        let (emb, att): (i64, i64) =
            f.q2("SELECT embedded, embed_attempts FROM turns WHERE id=?", &t);
        assert_eq!((emb, att), (0, 0));
        f.fake.set_failing("vector-db", false);
        f.worker.run_work(T0 + 10).await;
        assert_eq!(f.fake.doc_ids("chats-turns"), vec![t]);
    }

    #[tokio::test]
    async fn failing_turn_batch_does_not_skip_session_summaries() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        let bad = f.turn(&sid, "poison", "doc", T0);
        f.lock()
            .execute(
                "UPDATE sessions SET title='T', summary='weather talk', summarized_rev=rev WHERE id=?",
                [&sid],
            )
            .unwrap();
        f.fake.poison_doc(&bad);
        f.worker.run_work(T0 + 10).await;
        assert_eq!(f.fake.doc_ids("chats-sessions"), vec![sid]);
    }

    /// The session dies while vector-db is being written and its cleanup
    /// already finished: whatever may have reached vector-db must be queued
    /// for deletion again, also if the write was rejected or never answered.
    fn kill_session_and_finish_cleanup_once(f: &Fixture, sid: &str) {
        let (db, sid) = (std::sync::Arc::clone(&f.db), sid.to_string());
        let once = std::sync::atomic::AtomicBool::new(false);
        f.fake.on_call("vec_upsert_batch", move |_| {
            if once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let mut c = db.lock().unwrap();
            crate::store::delete(&mut c, &sid).unwrap();
            c.execute("DELETE FROM cascade WHERE session_id = ?", [&sid])
                .unwrap();
            c.execute("DELETE FROM sessions WHERE id = ?", [&sid])
                .unwrap();
        });
    }

    #[tokio::test]
    async fn dead_session_is_requeued_after_a_rejected_document() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        let bad = f.turn(&sid, "poison", "doc", T0);
        f.fake.poison_doc(&bad);
        kill_session_and_finish_cleanup_once(&f, &sid);
        f.worker.run_work(T0 + 10).await;
        let ids: String = f.q("SELECT turn_ids FROM cascade WHERE session_id=?", [&sid]);
        assert!(ids.contains(&bad), "requeued for vector deletion: {ids}");
    }

    #[tokio::test]
    async fn dead_session_is_requeued_when_the_backend_is_unreachable() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        let t = f.turn(&sid, "hello", "world", T0);
        f.fake.set_failing("vector-db", true);
        kill_session_and_finish_cleanup_once(&f, &sid);
        f.worker.run_work(T0 + 10).await;
        let ids: String = f.q("SELECT turn_ids FROM cascade WHERE session_id=?", [&sid]);
        assert!(
            ids.contains(&t),
            "a timed-out write counts as maybe written: {ids}"
        );
    }
}
