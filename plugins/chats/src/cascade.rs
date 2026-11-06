//! Delete cascade (queue side). The worker half is added with the cascade pass.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::json;

use crate::db::with_db;
use crate::embed::{SESSIONS, TURNS};
use crate::rpc::is_missing_plugin;
use crate::worker::Worker;

const SWEEP_EVERY_MS: i64 = 24 * 3600 * 1000;
const SWEEP_KEEP_MS: i64 = 7 * SWEEP_EVERY_MS;
const BACKOFF_BASE_MS: i64 = 60_000;
const BACKOFF_MAX_MS: i64 = 3_600_000;
const JOBS_PER_PASS: i64 = 10;

fn db_err(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

/// (Re)queue cleanup of vectors for `session_id`: used when the worker finds
/// that a session died while it was writing vectors. Merges with a pending
/// job and clears its `done` list so every step runs again.
pub fn requeue(conn: &mut Connection, session_id: &str, turn_ids: &[String]) -> Result<(), String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT turn_ids FROM cascade WHERE session_id = ?",
            [session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let mut ids: Vec<String> = existing
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for t in turn_ids {
        if !ids.contains(t) {
            ids.push(t.clone());
        }
    }
    tx.execute(
        "INSERT INTO cascade(session_id, turn_ids, goal_ids, attempts, next_at, done)
         VALUES (?1, ?2, '[]', 0, 0, '[]')
         ON CONFLICT(session_id) DO UPDATE SET turn_ids = ?2, attempts = 0, next_at = 0, done = '[]'",
        params![session_id, json!(ids).to_string()],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)
}

struct Job {
    session_id: String,
    turn_ids: Vec<String>,
    goal_ids: Vec<String>,
    attempts: i64,
    done: HashSet<String>,
}

fn due_jobs(conn: &Connection, now: i64) -> Result<Vec<Job>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT session_id, turn_ids, goal_ids, attempts, done FROM cascade
             WHERE next_at <= ? ORDER BY next_at LIMIT ?",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![now, JOBS_PER_PASS], |r| {
            let list = |s: String| serde_json::from_str::<Vec<String>>(&s).unwrap_or_default();
            Ok(Job {
                session_id: r.get(0)?,
                turn_ids: list(r.get(1)?),
                goal_ids: list(r.get(2)?),
                attempts: r.get(3)?,
                done: list(r.get(4)?).into_iter().collect(),
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?;
    Ok(rows)
}

/// Backoff after the `attempts`-th failure: 1 min, 2 min, 4 min ... capped at 1 h.
pub fn backoff_ms(attempts: i64) -> i64 {
    let exp = (attempts - 1).clamp(0, 20) as u32;
    (BACKOFF_BASE_MS << exp).min(BACKOFF_MAX_MS)
}

/// All steps done: drop the job and the session row, remember the vector ids
/// for the daily sweep (a worker write may still land after the cascade).
fn finish(conn: &mut Connection, job: &Job, now: i64) -> Result<(), String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    tx.execute(
        "DELETE FROM cascade WHERE session_id = ?",
        [&job.session_id],
    )
    .map_err(db_err)?;
    tx.execute(
        "DELETE FROM sessions WHERE id = ? AND state = 'deleting'",
        [&job.session_id],
    )
    .map_err(db_err)?;
    let old: Option<String> = tx
        .query_row(
            "SELECT turn_ids FROM swept WHERE session_id = ?",
            [&job.session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let mut ids: Vec<String> = old
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for t in &job.turn_ids {
        if !ids.contains(t) {
            ids.push(t.clone());
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO swept(session_id, turn_ids, until) VALUES (?, ?, ?)",
        params![job.session_id, json!(ids).to_string(), now + SWEEP_KEEP_MS],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)
}

fn save_progress(
    conn: &Connection,
    job: &Job,
    done: &HashSet<String>,
    now: i64,
) -> Result<(), String> {
    let mut d: Vec<&String> = done.iter().collect();
    d.sort();
    conn.execute(
        "UPDATE cascade SET attempts = ?, next_at = ?, done = ? WHERE session_id = ?",
        params![
            job.attempts + 1,
            now + backoff_ms(job.attempts + 1),
            json!(d).to_string(),
            job.session_id
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

impl Worker {
    /// Run due cascade jobs. Every step is idempotent and remembered in
    /// `done`; a step whose plugin is missing or failing stays open and the
    /// job is retried with backoff, indefinitely (a restarting plugin must not
    /// make the deletion get dropped). Returns true if a session row went away.
    pub(crate) async fn cascade_pass(&self, now: i64) -> bool {
        let jobs = match with_db(&self.db, move |c| due_jobs(c, now)).await {
            Ok(Ok(j)) => j,
            Ok(Err(e)) | Err(e) => {
                self.warn_once("cascade-db", &e);
                return false;
            }
        };
        let mut finished = false;
        for job in jobs {
            let mut done = job.done.clone();
            let mut failed = false;
            let mut down: HashSet<&'static str> = HashSet::new();
            let mut steps: Vec<(String, &'static str, &'static str, serde_json::Value)> =
                Vec::new();
            for t in &job.turn_ids {
                steps.push((
                    format!("t:{t}"),
                    "vector-db",
                    "vec_delete",
                    json!({"collection": TURNS, "id": t}),
                ));
            }
            steps.push((
                "s".into(),
                "vector-db",
                "vec_delete",
                json!({"collection": SESSIONS, "id": job.session_id}),
            ));
            for g in &job.goal_ids {
                steps.push((format!("g:{g}"), "agent", "goal_delete", json!({"id": g})));
            }
            steps.push((
                "m".into(),
                "agent",
                "memory_forget_session",
                json!({"session_id": job.session_id}),
            ));
            for (key, plugin, action, params) in steps {
                if done.contains(&key) || down.contains(plugin) {
                    if !done.contains(&key) {
                        failed = true;
                    }
                    continue;
                }
                match self.rpc.call(action, params, 30_000).await {
                    Ok(_) => {
                        done.insert(key);
                    }
                    // A missing plugin may only be restarting: its data is not
                    // known to be gone, so the step stays open (fail closed).
                    Err(e) if is_missing_plugin(&e) => {
                        self.warn_once(plugin, &e);
                        down.insert(plugin);
                        failed = true;
                    }
                    Err(e) => {
                        self.warn_once("cascade-step", &e);
                        down.insert(plugin);
                        failed = true;
                    }
                }
            }
            let r = with_db(&self.db, move |c| {
                if failed {
                    save_progress(c, &job, &done, now)
                } else {
                    finish(c, &job, now)
                }
                .map(|_| !failed)
            })
            .await;
            match r {
                Ok(Ok(ok)) => finished |= ok,
                Ok(Err(e)) | Err(e) => self.warn_once("cascade-db", &e),
            }
        }
        finished
    }

    /// Once a day repeat the vector deletes of recently finished cascades.
    pub(crate) async fn sweep_pass(&self, now: i64) {
        let due = with_db(&self.db, move |c| {
            let last: Option<i64> = c
                .query_row("SELECT value FROM meta WHERE key = 'last_sweep'", [], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(db_err)?;
            if last.is_some_and(|l| now - l < SWEEP_EVERY_MS) {
                return Ok(None);
            }
            let mut stmt = c
                .prepare("SELECT session_id, turn_ids, until FROM swept")
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(db_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_err)?;
            Ok::<_, String>(Some(rows))
        })
        .await;
        let rows = match due {
            Ok(Ok(Some(r))) => r,
            Ok(Ok(None)) => return,
            Ok(Err(e)) | Err(e) => return self.warn_once("sweep-db", &e),
        };
        let mut ok = true;
        for (sid, turn_ids, _) in &rows {
            let ids: Vec<String> = serde_json::from_str(turn_ids).unwrap_or_default();
            let mut calls = vec![(SESSIONS, sid.clone())];
            calls.extend(ids.into_iter().map(|t| (TURNS, t)));
            for (coll, id) in calls {
                match self
                    .rpc
                    .call("vec_delete", json!({"collection": coll, "id": id}), 30_000)
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        let key = if is_missing_plugin(&e) {
                            "vector-db"
                        } else {
                            "sweep-step"
                        };
                        self.warn_once(key, &e);
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok {
            return; // keep rows and retry on the next tick
        }
        let r = with_db(&self.db, move |c| {
            c.execute("DELETE FROM swept WHERE until < ?", [now])
                .map_err(db_err)?;
            c.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('last_sweep', ?)",
                [now],
            )
            .map_err(db_err)
        })
        .await;
        if let Ok(Err(e)) | Err(e) = r {
            self.warn_once("sweep-db", &e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Fixture, T0};
    use serde_json::Value;

    fn names(v: &[Value]) -> Vec<String> {
        v.iter()
            .map(|p| {
                p["id"]
                    .as_str()
                    .or(p["session_id"].as_str())
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    async fn deleted_session(f: &Fixture) -> (String, Vec<String>) {
        let sid = f.session("lap", "chat", T0);
        let t1 = f.turn_with_goal(&sid, "weather", "sunny", "g1", T0);
        let t2 = f.turn(&sid, "tomorrow", "rain", T0 + 1);
        f.lock()
            .execute("UPDATE sessions SET title='W', summary='weather talk', summarized_rev=rev WHERE id=?", [&sid])
            .unwrap();
        f.worker.run_once(T0 + 2).await; // embeds turns and summary
        assert_eq!(f.fake.doc_ids("chats-turns").len(), 2);
        f.delete(&sid).await;
        (sid, vec![t1, t2])
    }

    #[tokio::test]
    async fn delete_enqueues_cascade_with_turn_and_goal_ids() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        let t1 = f.turn_with_goal(&sid, "a", "b", "g1", T0);
        let t2 = f.turn_with_goal(&sid, "c", "d", "g1", T0 + 1);
        f.turn(&sid, "e", "f", T0 + 2);
        f.delete(&sid).await;
        let (turns, goals, next_at): (String, String, i64) = f.q2(
            "SELECT turn_ids, goal_ids, next_at FROM cascade WHERE session_id=?",
            &sid,
        );
        let turns: Vec<String> = serde_json::from_str(&turns).unwrap();
        assert_eq!(turns.len(), 3);
        assert!(turns.contains(&t1) && turns.contains(&t2));
        assert_eq!(goals, r#"["g1"]"#);
        assert_eq!(next_at, 0);
        assert_eq!(
            f.fake.call_count(),
            0,
            "delete itself makes no outbound call"
        );
    }

    #[tokio::test]
    async fn cascade_removes_vectors_goals_and_facts() {
        let f = Fixture::new();
        let (sid, turns) = deleted_session(&f).await;
        f.worker.run_once(T0 + 3).await;
        assert!(f.fake.doc_ids("chats-turns").is_empty());
        assert!(f.fake.doc_ids("chats-sessions").is_empty());
        let mut got = names(&f.fake.calls("vec_delete"));
        got.sort();
        let mut want = turns.clone();
        want.push(sid.clone());
        want.sort();
        assert_eq!(got, want);
        assert_eq!(f.fake.calls("goal_delete"), vec![json!({"id": "g1"})]);
        assert_eq!(
            f.fake.calls("memory_forget_session"),
            vec![json!({"session_id": sid})]
        );
        let sessions: i64 = f.q("SELECT count(*) FROM sessions WHERE id=?", [&sid]);
        let tomb: i64 = f.q("SELECT count(*) FROM tombstones WHERE id=?", [&sid]);
        let queue: i64 = f.q("SELECT count(*) FROM cascade", []);
        let swept: i64 = f.q("SELECT count(*) FROM swept WHERE session_id=?", [&sid]);
        assert_eq!((sessions, tomb, queue, swept), (0, 1, 0, 1));
        // finished: a further pass does nothing
        let n = f.fake.call_count();
        f.worker.run_once(T0 + 4).await;
        assert_eq!(f.fake.call_count(), n);
    }

    #[tokio::test]
    async fn cascade_retries_with_backoff_when_agent_down() {
        let f = Fixture::new();
        let (sid, _) = deleted_session(&f).await;
        f.fake.set_failing("agent", true);
        let now = T0 + 10;
        f.worker.run_once(now).await;
        let (attempts, next_at): (i64, i64) = f.q2(
            "SELECT attempts, next_at FROM cascade WHERE session_id=?",
            &sid,
        );
        assert_eq!((attempts, next_at), (1, now + 60_000));
        assert_eq!(f.fake.calls("vec_delete").len(), 3);
        assert_eq!(f.fake.calls("goal_delete").len(), 1);
        let sessions: i64 = f.q("SELECT count(*) FROM sessions WHERE id=?", [&sid]);
        assert_eq!(
            sessions, 1,
            "session row stays until the cascade is complete"
        );
        // not due yet: nothing happens
        let n = f.fake.call_count();
        f.worker.run_once(now + 1000).await;
        assert_eq!(f.fake.call_count(), n);
        // due, still down: attempts 2, backoff doubles
        f.worker.run_once(now + 60_000).await;
        let (attempts, next_at): (i64, i64) = f.q2(
            "SELECT attempts, next_at FROM cascade WHERE session_id=?",
            &sid,
        );
        assert_eq!((attempts, next_at), (2, now + 60_000 + 120_000));
        // agent comes back
        f.fake.set_failing("agent", false);
        f.worker.run_once(now + 180_000).await;
        let queue: i64 = f.q("SELECT count(*) FROM cascade", []);
        assert_eq!(queue, 0);
        assert_eq!(f.fake.calls("goal_delete").len(), 3);
        assert_eq!(f.fake.calls("memory_forget_session").len(), 1);
        // vector deletes were not repeated: finished steps are remembered
        assert_eq!(f.fake.calls("vec_delete").len(), 3);
    }

    #[test]
    fn backoff_doubles_up_to_one_hour() {
        assert_eq!(backoff_ms(1), 60_000);
        assert_eq!(backoff_ms(2), 120_000);
        assert_eq!(backoff_ms(5), 960_000);
        assert_eq!(backoff_ms(7), 3_600_000);
        assert_eq!(backoff_ms(50), 3_600_000);
    }

    #[tokio::test]
    async fn missing_plugin_step_is_retried_until_plugin_returns() {
        let f = Fixture::new();
        let (sid, _) = deleted_session(&f).await;
        f.fake.set_missing("agent", true);
        f.fake.set_missing("vector-db", true);
        let now = T0 + 10;
        f.worker.run_once(now).await;
        let (sessions, queue, attempts): (i64, i64, i64) = (
            f.q("SELECT count(*) FROM sessions WHERE id=?", [&sid]),
            f.q("SELECT count(*) FROM cascade", []),
            f.q("SELECT attempts FROM cascade", []),
        );
        assert_eq!(
            (sessions, queue, attempts),
            (1, 1, 1),
            "job kept, not dropped"
        );
        // keeps retrying with growing backoff, far beyond any fixed limit
        let mut t = now;
        for _ in 0..12 {
            t += 3_600_000;
            f.worker.run_once(t).await;
        }
        let queue: i64 = f.q("SELECT count(*) FROM cascade", []);
        let next_at: i64 = f.q("SELECT next_at FROM cascade", []);
        assert_eq!(queue, 1);
        assert_eq!(next_at, t + 3_600_000, "backoff capped at 1 h");
        assert!(f.fake.calls("goal_delete").len() >= 12);
        // plugins return: everything is deleted, job removed
        f.fake.set_missing("agent", false);
        f.fake.set_missing("vector-db", false);
        f.worker.run_once(t + 3_600_000).await;
        let (sessions, queue): (i64, i64) = (
            f.q("SELECT count(*) FROM sessions WHERE id=?", [&sid]),
            f.q("SELECT count(*) FROM cascade", []),
        );
        assert_eq!((sessions, queue), (0, 0));
        assert!(f.fake.doc_ids("chats-turns").is_empty());
        assert!(f.fake.doc_ids("chats-sessions").is_empty());
        assert!(!f.fake.calls("memory_forget_session").is_empty());
    }

    #[tokio::test]
    async fn embed_after_delete_is_cleaned() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        f.turn(&sid, "weather", "sunny", T0);
        let (db, s2) = (std::sync::Arc::clone(&f.db), sid.clone());
        // the session is deleted while the worker waits for vector-db
        f.fake.on_call("vec_upsert_batch", move |_| {
            let mut c = db.lock().unwrap();
            crate::store::delete(&mut c, &s2).unwrap();
        });
        f.worker.run_once(T0 + 1).await;
        assert!(
            f.fake.doc_ids("chats-turns").is_empty(),
            "late vector must be cleaned"
        );
        let sessions: i64 = f.q("SELECT count(*) FROM sessions WHERE id=?", [&sid]);
        assert_eq!(sessions, 0);
    }

    #[tokio::test]
    async fn requeue_merges_and_resets_done() {
        let f = Fixture::new();
        let sid = f.session("lap", "chat", T0);
        f.turn(&sid, "a", "b", T0);
        f.delete(&sid).await;
        f.lock()
            .execute(
                "UPDATE cascade SET done='[\"s\"]', attempts=3, next_at=99 WHERE session_id=?",
                [&sid],
            )
            .unwrap();
        requeue(&mut f.lock(), &sid, &["late".to_string()]).unwrap();
        let (turns, done, attempts, next_at): (String, String, i64, i64) = f.q2(
            "SELECT turn_ids, done, attempts, next_at FROM cascade WHERE session_id=?",
            &sid,
        );
        let turns: Vec<String> = serde_json::from_str(&turns).unwrap();
        assert_eq!(turns.len(), 2);
        assert!(turns.contains(&"late".to_string()));
        assert_eq!((done.as_str(), attempts, next_at), ("[]", 0, 0));
    }

    #[tokio::test]
    async fn sweep_deletes_late_vectors() {
        let f = Fixture::new();
        let (sid, turns) = deleted_session(&f).await;
        f.worker.run_once(T0 + 3).await; // cascade done
        let day = 24 * 3600 * 1000;
        // a straggling worker write lands after the cascade
        f.fake
            .handle(
                "vec_upsert_batch",
                json!({"collection": "chats-turns", "docs": [{"id": turns[0], "text": "late", "metadata": {}}]}),
            )
            .await
            .unwrap();
        assert_eq!(f.fake.doc_ids("chats-turns").len(), 1);
        f.worker.run_once(T0 + 3 + 1000).await; // same day: no sweep (last_sweep set on first run)
        assert_eq!(f.fake.doc_ids("chats-turns").len(), 1);
        f.worker.run_once(T0 + 3 + day + 10).await;
        assert!(f.fake.doc_ids("chats-turns").is_empty());
        // entries expire after a week
        f.worker.run_once(T0 + 3 + 9 * day).await;
        let swept: i64 = f.q("SELECT count(*) FROM swept WHERE session_id=?", [&sid]);
        assert_eq!(swept, 0);
    }
}
