//! Delete cascade (queue side). The worker half is added with the cascade pass.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::json;

use crate::db::with_db;
use crate::embed::{SESSIONS, TURNS};
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
         ON CONFLICT(session_id) DO UPDATE SET turn_ids = ?2, attempts = 0, next_at = 0, done = '[]',
                                               gen = gen + 1",
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
    gen: i64,
}

fn due_jobs(conn: &Connection, now: i64) -> Result<Vec<Job>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT session_id, turn_ids, goal_ids, attempts, done, gen FROM cascade
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
                gen: r.get(5)?,
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
///
/// Returns false (and changes nothing) if the job was requeued since it was
/// read: its `gen` moved, so the new vector ids still need deleting.
fn finish(conn: &mut Connection, job: &Job, now: i64) -> Result<bool, String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let removed = tx
        .execute(
            "DELETE FROM cascade WHERE session_id = ? AND gen = ?",
            params![job.session_id, job.gen],
        )
        .map_err(db_err)?;
    if removed == 0 {
        return Ok(false);
    }
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
        "INSERT OR REPLACE INTO swept(session_id, turn_ids, until, swept_at) VALUES (?, ?, ?, 0)",
        params![job.session_id, json!(ids).to_string(), now + SWEEP_KEEP_MS],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(true)
}

/// Remember finished steps. `backoff` = a step failed: count the attempt and
/// schedule the retry; otherwise (pass ran out of budget) the job stays due.
fn save_progress(
    conn: &Connection,
    job: &Job,
    done: &HashSet<String>,
    now: i64,
    backoff: bool,
) -> Result<(), String> {
    let mut d: Vec<&String> = done.iter().collect();
    d.sort();
    let (attempts, next_at) = if backoff {
        (job.attempts + 1, now + backoff_ms(job.attempts + 1))
    } else {
        (job.attempts, 0)
    };
    conn.execute(
        "UPDATE cascade SET attempts = ?, next_at = ?, done = ? WHERE session_id = ? AND gen = ?",
        params![
            attempts,
            next_at,
            json!(d).to_string(),
            job.session_id,
            job.gen
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

type Step = (String, &'static str, &'static str, serde_json::Value);

fn steps_of(job: &Job) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
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
    steps
}

impl Worker {
    /// Run due cascade jobs. Every step is idempotent and remembered in
    /// `done`; a step whose plugin is missing or failing stays open and the
    /// job is retried with backoff, indefinitely (a restarting plugin must not
    /// make the deletion get dropped). A plugin that fails once is not called
    /// again in this pass, and the pass stops after its wall-clock budget, so
    /// a sick plugin costs one timeout, not one per id. Returns true if a
    /// session row went away.
    pub(crate) async fn cascade_pass(&self, now: i64) -> bool {
        let jobs = match with_db(&self.db, move |c| due_jobs(c, now)).await {
            Ok(Ok(j)) => j,
            Ok(Err(e)) | Err(e) => {
                self.warn("db", &e);
                return false;
            }
        };
        let started = std::time::Instant::now();
        let budget = self.pass_budget();
        let mut down: HashSet<&'static str> = HashSet::new();
        let mut finished = false;
        // at least one call per pass, whatever the budget: progress is guaranteed
        let mut calls = 0u32;
        for job in jobs {
            if calls > 0 && started.elapsed() >= budget {
                break; // the rest stays due for the next pass
            }
            let mut done = job.done.clone();
            let mut failed = false;
            let mut out_of_budget = false;
            for (key, plugin, action, params) in steps_of(&job) {
                if done.contains(&key) {
                    continue;
                }
                if down.contains(plugin) {
                    failed = true;
                    continue;
                }
                if calls > 0 && started.elapsed() >= budget {
                    out_of_budget = true;
                    continue;
                }
                calls += 1;
                match self.rpc.call(action, params, 30_000).await {
                    Ok(_) => {
                        self.ok(plugin);
                        done.insert(key);
                    }
                    // A missing plugin may only be restarting: its data is not
                    // known to be gone, so the step stays open (fail closed).
                    Err(e) => {
                        self.warn(plugin, &e);
                        down.insert(plugin);
                        failed = true;
                    }
                }
            }
            let incomplete = failed || out_of_budget;
            let r = with_db(&self.db, move |c| {
                if incomplete {
                    save_progress(c, &job, &done, now, failed).map(|_| false)
                } else {
                    finish(c, &job, now)
                }
            })
            .await;
            match r {
                Ok(Ok(ok)) => finished |= ok,
                Ok(Err(e)) | Err(e) => {
                    self.warn("db", &e);
                }
            }
        }
        finished
    }

    /// Once a day repeat the vector deletes of recently finished cascades.
    ///
    /// A sweep *cycle* (start time kept in `meta.sweep_cycle`) visits every
    /// `swept` row once: a row is stamped `swept_at = cycle` when all its
    /// deletes worked, so a partial pass (budget, failure) resumes where it
    /// stopped instead of starting over. A failing row is skipped and retried
    /// next pass; three failures in a row end the pass (backend down). Expired
    /// rows are pruned on every pass.
    pub(crate) async fn sweep_pass(&self, now: i64) {
        let plan = with_db(&self.db, move |c| sweep_plan(c, now)).await;
        let (cycle, rows) = match plan {
            Ok(Ok(Some(p))) => p,
            Ok(Ok(None)) => return,
            Ok(Err(e)) | Err(e) => {
                self.warn("db", &e);
                return;
            }
        };
        let started = std::time::Instant::now();
        let budget = self.pass_budget();
        let mut swept: Vec<(String, String)> = Vec::new();
        let mut failures = 0;
        'rows: for (n, (sid, turn_ids)) in rows.iter().enumerate() {
            if n > 0 && started.elapsed() >= budget {
                break;
            }
            let ids: Vec<String> = serde_json::from_str(turn_ids).unwrap_or_default();
            let mut calls = vec![(SESSIONS, sid.clone())];
            calls.extend(ids.into_iter().map(|t| (TURNS, t)));
            for (coll, id) in calls {
                match self
                    .rpc
                    .call("vec_delete", json!({"collection": coll, "id": id}), 30_000)
                    .await
                {
                    Ok(_) => self.ok("vector-db"),
                    Err(e) => {
                        self.warn("vector-db", &e);
                        failures += 1;
                        if failures >= 3 {
                            break 'rows;
                        }
                        continue 'rows; // retried in a later pass
                    }
                }
            }
            failures = 0;
            swept.push((sid.clone(), turn_ids.clone()));
        }
        if swept.is_empty() {
            return;
        }
        let r = with_db(&self.db, move |c| {
            let tx = c.transaction().map_err(db_err)?;
            for (sid, turn_ids) in &swept {
                // only if the row was not rewritten (new late ids) meanwhile
                tx.execute(
                    "UPDATE swept SET swept_at = ?1 WHERE session_id = ?2 AND turn_ids = ?3",
                    params![cycle, sid, turn_ids],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)
        })
        .await;
        if let Ok(Err(e)) | Err(e) = r {
            self.warn("db", &e);
        }
    }
}

type SweepRows = Vec<(String, String)>;

/// Prune expired rows, open or continue a cycle and pick the rows still to
/// visit; closes the cycle when none is left.
fn sweep_plan(c: &Connection, now: i64) -> Result<Option<(i64, SweepRows)>, String> {
    let meta = |key: &str| -> Result<Option<i64>, String> {
        c.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0))
            .optional()
            .map_err(db_err)
    };
    c.execute("DELETE FROM swept WHERE until < ?", [now])
        .map_err(db_err)?;
    let cycle = match meta("sweep_cycle")? {
        Some(cy) => cy,
        None => {
            if meta("last_sweep")?.is_some_and(|l| now - l < SWEEP_EVERY_MS) {
                return Ok(None);
            }
            c.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('sweep_cycle', ?)",
                [now],
            )
            .map_err(db_err)?;
            now
        }
    };
    let rows: SweepRows = c
        .prepare(
            "SELECT session_id, turn_ids FROM swept WHERE swept_at < ?
             ORDER BY swept_at, session_id LIMIT 200",
        )
        .map_err(db_err)?
        .query_map([cycle], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err)?;
    if rows.is_empty() {
        c.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('last_sweep', ?)",
            [cycle],
        )
        .map_err(db_err)?;
        c.execute("DELETE FROM meta WHERE key = 'sweep_cycle'", [])
            .map_err(db_err)?;
        return Ok(None);
    }
    Ok(Some((cycle, rows)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Fixture, T0};
    use serde_json::Value;
    use std::time::Duration;

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

    /// `n` deleted sessions, each with one turn and a goal, vectors embedded.
    async fn deleted_sessions(f: &Fixture, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0..n {
            let sid = f.session(&format!("dev{i}"), "chat", T0);
            f.turn_with_goal(&sid, "weather", "sunny", &format!("g{i}"), T0);
            out.push(sid);
        }
        f.worker.run_once(T0 + 2).await;
        for sid in &out {
            f.delete(sid).await;
        }
        out
    }

    #[tokio::test]
    async fn cleanup_lane_is_not_blocked_by_a_hung_ai() {
        let f = Fixture::new();
        let (sid, _) = deleted_session(&f).await;
        // another idle session makes the work lane call a very slow `ai`
        let other = f.session("pc", "chat", T0);
        f.turn(&other, "hello", "world", T0);
        f.fake.set_delay("ai", 3_000);
        f.fake.script_completion(Ok("{}"));
        let w = std::sync::Arc::clone(&f.worker);
        let work = tokio::spawn(async move { w.run_work(T0 + 1_000_000).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            f.fake.calls("chat_completion").len(),
            1,
            "work lane is inside the ai call"
        );
        let t = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(1), f.worker.run_cleanup(T0 + 10))
            .await
            .expect("cascade must not wait for the work lane");
        assert!(t.elapsed() < Duration::from_secs(1));
        let rows: i64 = f.q("SELECT count(*) FROM sessions WHERE id=?", [&sid]);
        assert_eq!(rows, 0, "cascade finished while ai was still hanging");
        assert!(!work.is_finished());
        work.abort();
    }

    #[tokio::test]
    async fn failing_plugin_is_called_once_per_pass() {
        let f = Fixture::new();
        deleted_sessions(&f, 3).await;
        f.fake.set_failing("agent", true);
        f.worker.run_cleanup(T0 + 10).await;
        assert_eq!(f.fake.calls("goal_delete").len(), 1, "no call per id");
        assert!(f.fake.calls("memory_forget_session").is_empty());
        // vector-db was healthy: its steps are done for all three sessions
        assert_eq!(f.fake.calls("vec_delete").len(), 6);
        let queue: i64 = f.q("SELECT count(*) FROM cascade", []);
        assert_eq!(queue, 3);
    }

    #[tokio::test]
    async fn pass_stops_at_its_wall_clock_budget() {
        let f = Fixture::new();
        deleted_sessions(&f, 3).await;
        f.worker.set_pass_budget_ms(0);
        let before = f.fake.call_count();
        f.worker.run_cleanup(T0 + 10).await;
        assert_eq!(
            f.fake.call_count() - before,
            1,
            "budget 0 = one call per pass"
        );
        let queue: i64 = f.q("SELECT count(*) FROM cascade", []);
        assert_eq!(queue, 3);
        // the cut jobs stay due (no backoff): enough passes finish everything
        for _ in 0..20 {
            f.worker.run_cleanup(T0 + 10).await;
        }
        let queue: i64 = f.q("SELECT count(*) FROM cascade", []);
        assert_eq!(queue, 0);
    }

    fn swept_turns(f: &Fixture, sid: &str) -> Vec<String> {
        let t: String = f.q("SELECT turn_ids FROM swept WHERE session_id=?", [sid]);
        serde_json::from_str(&t).unwrap()
    }

    async fn put_late_vector(f: &Fixture, id: &str) {
        f.fake
            .handle(
                "vec_upsert_batch",
                json!({"collection": "chats-turns", "docs": [{"id": id, "text": "late", "metadata": {}}]}),
            )
            .await
            .unwrap();
    }

    const DAY: i64 = 24 * 3600 * 1000;

    #[tokio::test]
    async fn sweep_skips_a_failing_row_and_resumes_next_pass() {
        let f = Fixture::new();
        let sids = deleted_sessions(&f, 3).await;
        f.worker.run_cleanup(T0 + 3).await; // cascades done, first cycle complete
        let mut late = Vec::new();
        for sid in &sids {
            let id = swept_turns(&f, sid)[0].clone();
            put_late_vector(&f, &id).await;
            late.push(id);
        }
        let last0: i64 = f.q("SELECT value FROM meta WHERE key='last_sweep'", []);
        f.fake.poison_doc(&late[0]); // vec_delete of this one fails
        let day2 = T0 + 3 + DAY + 10;
        f.worker.run_cleanup(day2).await;
        let left = f.fake.doc_ids("chats-turns");
        assert_eq!(
            left,
            vec![late[0].clone()],
            "the other rows were swept past the failing one"
        );
        let cycle: i64 = f.q("SELECT value FROM meta WHERE key='sweep_cycle'", []);
        let last: i64 = f.q("SELECT value FROM meta WHERE key='last_sweep'", []);
        assert_eq!((cycle, last), (day2, last0), "cycle stays open");
        // backend fine again: the next pass finishes the row, one more closes the cycle
        f.fake.with(|s| s.poison.clear());
        f.worker.run_cleanup(day2 + 1000).await;
        assert!(f.fake.doc_ids("chats-turns").is_empty());
        f.worker.run_cleanup(day2 + 2000).await;
        let last: i64 = f.q("SELECT value FROM meta WHERE key='last_sweep'", []);
        let open: i64 = f.q("SELECT count(*) FROM meta WHERE key='sweep_cycle'", []);
        assert_eq!((last, open), (day2, 0));
    }

    #[tokio::test]
    async fn sweep_partial_pass_keeps_its_place_and_prunes_expired_rows() {
        let f = Fixture::new();
        deleted_sessions(&f, 3).await;
        f.worker.run_cleanup(T0 + 3).await;
        f.lock()
            .execute(
                "INSERT INTO swept(session_id, turn_ids, until) VALUES('old','[]',1)",
                [],
            )
            .unwrap();
        f.worker.set_pass_budget_ms(0); // one row per pass
        let day2 = T0 + 3 + DAY + 10;
        let before = f.fake.calls("vec_delete").len();
        f.worker.run_cleanup(day2).await;
        let old: i64 = f.q("SELECT count(*) FROM swept WHERE session_id='old'", []);
        let stamped: i64 = f.q("SELECT count(*) FROM swept WHERE swept_at = ?", [day2]);
        assert_eq!((old, stamped), (0, 1), "expired row pruned, one row swept");
        for _ in 0..3 {
            f.worker.run_cleanup(day2 + 1).await;
        }
        let stamped: i64 = f.q("SELECT count(*) FROM swept WHERE swept_at = ?", [day2]);
        assert_eq!(stamped, 3);
        // every row visited exactly once (2 deletes each): no restarts from the top
        assert_eq!(f.fake.calls("vec_delete").len() - before, 6);
        let last: i64 = f.q("SELECT value FROM meta WHERE key='last_sweep'", []);
        assert_eq!(last, day2);
    }

    #[tokio::test]
    async fn job_requeued_while_cascade_runs_is_not_finished() {
        let f = Fixture::new();
        let (sid, _) = deleted_session(&f).await;
        let (db, s2) = (std::sync::Arc::clone(&f.db), sid.clone());
        let once = std::sync::atomic::AtomicBool::new(false);
        // the work lane discovers a late vector exactly while cleanup runs
        f.fake.on_call("memory_forget_session", move |_| {
            if !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                requeue(&mut db.lock().unwrap(), &s2, &["late".to_string()]).unwrap();
            }
        });
        f.worker.run_cleanup(T0 + 3).await;
        let queue: i64 = f.q("SELECT count(*) FROM cascade WHERE session_id=?", [&sid]);
        let sessions: i64 = f.q("SELECT count(*) FROM sessions WHERE id=?", [&sid]);
        let (done, gen): (String, i64) =
            f.q2("SELECT done, gen FROM cascade WHERE session_id=?", &sid);
        assert_eq!(
            (queue, sessions, done.as_str(), gen),
            (1, 1, "[]", 1),
            "job kept for another round"
        );
        f.worker.run_cleanup(T0 + 4).await;
        let queue: i64 = f.q("SELECT count(*) FROM cascade", []);
        assert_eq!(queue, 0);
        let ids = names(&f.fake.calls("vec_delete"));
        assert!(
            ids.contains(&"late".to_string()),
            "the late id was deleted: {ids:?}"
        );
    }
}
