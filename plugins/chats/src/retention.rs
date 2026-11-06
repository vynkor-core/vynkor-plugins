//! Retention: usage tracking, automatic junk cleanup, compaction of old
//! unused sessions and the monthly owner review.
//!
//! Everything here is synchronous SQLite work. Deletions and compactions only
//! *enqueue* cleanup in the `cascade` table; the cleanup lane of the worker
//! (which runs the maintenance pass first) performs the calls to other plugins.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::json;

use crate::cascade::MODE_COMPACT;
use crate::config::Config;
use crate::db::with_db;
use crate::store::{self, next_rev};
use crate::worker::Worker;

/// Sessions handled per maintenance pass and rule.
const PASS_LIMIT: i64 = 200;

fn db_err(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

/// Record that sessions were surfaced to the user or the agent. `recall`
/// (a `chat_recall` hit) also counts towards `recall_count`. Deliberately does
/// not touch `rev`: usage is host-local and never synced.
pub fn mark_used(
    conn: &Connection,
    session_ids: &[String],
    now: i64,
    recall: bool,
) -> Result<(), String> {
    for id in session_ids {
        conn.execute(
            "UPDATE sessions SET last_used_at = ?1, recall_count = recall_count + ?2
             WHERE id = ?3 AND state = 'live'",
            params![now, i64::from(recall), id],
        )
        .map_err(db_err)?;
    }
    Ok(())
}

/// Live, unpinned sessions older than `CHATS_JUNK_AGE_SECS` in which *every*
/// turn is junk (not `ok`, or an empty question or answer), including
/// sessions without turns. Summarized (`tier = summary`) sessions have no turn
/// rows by design and are never junk.
pub fn junk_sessions(conn: &Connection, now: i64, cfg: &Config) -> Result<Vec<String>, String> {
    let before = now - cfg.junk_age_secs.saturating_mul(1000);
    conn.prepare(
        "SELECT s.id FROM sessions s
         WHERE s.state = 'live' AND s.pinned = 0 AND s.tier != 'summary' AND s.last_at < ?1
           AND NOT EXISTS (
                SELECT 1 FROM turns t WHERE t.session_id = s.id AND t.status = 'ok'
                   AND trim(t.user_text) != '' AND trim(t.answer_text) != '')
         ORDER BY s.last_at LIMIT ?2",
    )
    .map_err(db_err)?
    .query_map(params![before, PASS_LIMIT], |r| r.get(0))
    .map_err(db_err)?
    .collect::<rusqlite::Result<_>>()
    .map_err(db_err)
}

/// `full` sessions that have a summary, are older than `CHATS_COMPACT_AGE_SECS`,
/// were not surfaced for `CHATS_COMPACT_UNUSED_SECS`, are unpinned and have no
/// cleanup job pending.
pub fn compact_candidates(
    conn: &Connection,
    now: i64,
    cfg: &Config,
) -> Result<Vec<String>, String> {
    let old = now - cfg.compact_age_secs.saturating_mul(1000);
    let unused = now - cfg.compact_unused_secs.saturating_mul(1000);
    conn.prepare(
        "SELECT s.id FROM sessions s
         WHERE s.state = 'live' AND s.pinned = 0 AND s.tier = 'full'
           AND s.summarized_rev > 0 AND s.summary != ''
           AND s.last_at < ?1 AND MAX(s.last_used_at, s.last_at) < ?2
           AND NOT EXISTS (SELECT 1 FROM cascade c WHERE c.session_id = s.id)
         ORDER BY s.last_at LIMIT 100",
    )
    .map_err(db_err)?
    .query_map(params![old, unused], |r| r.get(0))
    .map_err(db_err)?
    .collect::<rusqlite::Result<_>>()
    .map_err(db_err)
}

/// `full -> compact`: drop the tool traces of the turns, mark the session
/// `compact` and queue the removal of the turn vectors and agent goals. The
/// summary, tags, summary vector and facts stay. One transaction; returns
/// false if the session no longer qualifies (not live / pinned / not full /
/// cleanup pending).
pub fn compact(conn: &mut Connection, session_id: &str) -> Result<bool, String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let ok: bool = tx
        .query_row(
            "SELECT 1 FROM sessions s WHERE s.id = ?1 AND s.state = 'live' AND s.pinned = 0
               AND s.tier = 'full'
               AND NOT EXISTS (SELECT 1 FROM cascade c WHERE c.session_id = s.id)",
            [session_id],
            |_| Ok(true),
        )
        .optional()
        .map_err(db_err)?
        .unwrap_or(false);
    if !ok {
        return Ok(false);
    }
    let turn_ids: Vec<String> = tx
        .prepare("SELECT id FROM turns WHERE session_id = ? ORDER BY seq")
        .map_err(db_err)?
        .query_map([session_id], |r| r.get(0))
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err)?;
    let goal_ids: Vec<String> = tx
        .prepare(
            "SELECT DISTINCT goal_id FROM turns
             WHERE session_id = ? AND goal_id IS NOT NULL AND goal_id != ''",
        )
        .map_err(db_err)?
        .query_map([session_id], |r| r.get(0))
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err)?;
    let rev = next_rev(&tx).map_err(db_err)?;
    tx.execute(
        "UPDATE turns SET tool_trace = '[]', rev = ?1 WHERE session_id = ?2 AND tool_trace != '[]'",
        params![rev, session_id],
    )
    .map_err(db_err)?;
    // their vectors are being removed: never embed them again
    tx.execute(
        "UPDATE turns SET embedded = 1 WHERE session_id = ?1 AND embedded = 0",
        [session_id],
    )
    .map_err(db_err)?;
    tx.execute(
        "UPDATE sessions SET tier = 'compact', rev = ?1 WHERE id = ?2",
        params![rev, session_id],
    )
    .map_err(db_err)?;
    tx.execute(
        "INSERT INTO cascade(session_id, turn_ids, goal_ids, attempts, next_at, done, mode)
         VALUES (?1, ?2, ?3, 0, 0, '[]', ?4)",
        params![
            session_id,
            json!(turn_ids).to_string(),
            json!(goal_ids).to_string(),
            MODE_COMPACT
        ],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(true)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub deleted: usize,
    pub compacted: usize,
}

impl Outcome {
    pub fn changed(&self) -> bool {
        self.deleted + self.compacted > 0
    }
}

fn meta_get(conn: &Connection, key: &str) -> Result<Option<i64>, String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0))
        .optional()
        .map_err(db_err)
}

fn meta_set(conn: &Connection, key: &str, v: i64) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?, ?)",
        params![key, v],
    )
    .map_err(db_err)?;
    Ok(())
}

/// One maintenance pass; a no-op until `CHATS_MAINT_INTERVAL_SECS` has passed
/// since the previous one (`meta.last_maint`).
pub fn maintenance(conn: &mut Connection, cfg: &Config, now: i64) -> Result<Outcome, String> {
    let mut out = Outcome::default();
    if !cfg.retention {
        return Ok(out);
    }
    if meta_get(conn, "last_maint")?
        .is_some_and(|l| now - l < cfg.maint_interval_secs.saturating_mul(1000))
    {
        return Ok(out);
    }
    meta_set(conn, "last_maint", now)?;
    for id in junk_sessions(conn, now, cfg)? {
        if store::delete(conn, &id)?["deleted"] == true {
            out.deleted += 1;
        }
    }
    for id in compact_candidates(conn, now, cfg)? {
        if compact(conn, &id)? {
            out.compacted += 1;
        }
    }
    Ok(out)
}

impl Worker {
    /// Run the maintenance pass; true if synced state changed. The cleanup the
    /// pass enqueues is processed by the cascade pass that follows it.
    pub(crate) async fn maintenance_pass(&self, now: i64) -> bool {
        let cfg = std::sync::Arc::clone(&self.cfg);
        match with_db(&self.db, move |c| maintenance(c, &cfg, now)).await {
            Ok(Ok(o)) => o.changed(),
            Ok(Err(e)) | Err(e) => {
                self.warn("db", &e);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Fixture, T0};

    const DAY: i64 = 86_400_000;

    fn turn_status(f: &Fixture, sid: &str, user: &str, answer: &str, status: &str, at: i64) {
        let mut p = store::tests::ap(sid, user, answer);
        p.status = status.into();
        store::append(&mut f.lock(), &f.cfg, p, at).unwrap();
    }

    fn state(f: &Fixture, sid: &str) -> String {
        f.q("SELECT state FROM sessions WHERE id=?", [sid])
    }

    #[tokio::test]
    async fn junk_rule_deletes_only_old_all_junk_unpinned() {
        let f = Fixture::new();
        let now = T0 + 2 * DAY;
        // A: old, every turn junk (failed / empty answer) -> deleted
        let a = f.session("a", "chat", T0);
        turn_status(&f, &a, "hi", "", "ok", T0);
        turn_status(&f, &a, "again", "boom", "failed", T0 + 1);
        // B: old, one failed and one ok turn -> kept
        let b = f.session("b", "chat", T0);
        turn_status(&f, &b, "q", "a", "ok", T0);
        turn_status(&f, &b, "q2", "x", "failed", T0 + 1);
        // C: old junk but pinned -> kept
        let c = f.session("c", "chat", T0);
        turn_status(&f, &c, "q", "", "failed", T0);
        store::update(&mut f.lock(), &c, None, Some(true)).unwrap();
        // D: junk but younger than JUNK_AGE -> kept
        let d = f.session("d", "chat", now - 1000);
        turn_status(&f, &d, "q", "", "failed", now - 1000);
        let out = maintenance(&mut f.lock(), &f.cfg, now).unwrap();
        assert_eq!(out.deleted, 1);
        assert_eq!(state(&f, &a), "deleting");
        for keep in [&b, &c, &d] {
            assert_eq!(state(&f, keep), "live");
        }
    }

    #[tokio::test]
    async fn junk_rule_keeps_summarized_and_deletes_empty_sessions() {
        let f = Fixture::new();
        let now = T0 + 2 * DAY;
        let empty = f.session("e", "chat", T0); // no turns at all
        let summarized = f.session("s", "chat", T0);
        f.lock()
            .execute(
                "UPDATE sessions SET tier='summary', summary='kept' WHERE id=?",
                [&summarized],
            )
            .unwrap();
        maintenance(&mut f.lock(), &f.cfg, now).unwrap();
        assert_eq!(state(&f, &empty), "deleting");
        assert_eq!(state(&f, &summarized), "live");
    }

    #[tokio::test]
    async fn junk_delete_goes_through_cascade() {
        let f = Fixture::new();
        let sid = f.session("a", "chat", T0);
        let mut p = store::tests::ap(&sid, "q", "boom");
        p.status = "failed".into();
        p.goal_id = Some("g1".into());
        let t = store::append(&mut f.lock(), &f.cfg, p, T0).unwrap()["turn_id"]
            .as_str()
            .unwrap()
            .to_string();
        f.worker.run_work(T0 + 10).await; // embeds the turn
        assert_eq!(f.fake.doc_ids("chats-turns"), vec![t.clone()]);
        f.worker.run_cleanup(T0 + 2 * DAY).await;
        assert!(f.fake.doc_ids("chats-turns").is_empty());
        assert_eq!(
            f.fake.calls("goal_delete"),
            vec![serde_json::json!({"id": "g1"})]
        );
        assert_eq!(
            f.fake.calls("memory_forget_session"),
            vec![serde_json::json!({"session_id": sid})]
        );
        let rows: i64 = f.q("SELECT count(*) FROM sessions WHERE id=?", [&sid]);
        let tomb: i64 = f.q("SELECT count(*) FROM tombstones WHERE id=?", [&sid]);
        assert_eq!((rows, tomb), (0, 1));
    }

    #[tokio::test]
    async fn maintenance_runs_once_per_interval() {
        let f = Fixture::new();
        let a = f.session("a", "chat", T0);
        turn_status(&f, &a, "q", "", "failed", T0);
        let b = f.session("b", "chat", T0 + DAY);
        turn_status(&f, &b, "q", "", "failed", T0 + DAY);
        let t1 = T0 + 2 * DAY;
        // first pass: only A is old enough; the interval stamp is set
        f.worker.run_cleanup(t1).await;
        assert_eq!(
            f.q::<i64>("SELECT count(*) FROM sessions WHERE id=?", [&a]),
            0
        );
        assert_eq!(state(&f, &b), "live");
        // B is old enough now, but the interval (1 h) has not passed
        f.worker.run_cleanup(t1 + 3_000_000).await;
        assert_eq!(state(&f, &b), "live");
        f.worker.run_cleanup(t1 + 3_600_000).await;
        assert_eq!(
            f.q::<i64>("SELECT count(*) FROM sessions WHERE id=?", [&b]),
            0
        );
    }

    #[tokio::test]
    async fn retention_off_does_nothing() {
        let f = Fixture::with_config(|c| c.retention = false);
        let a = f.session("a", "chat", T0);
        turn_status(&f, &a, "q", "", "failed", T0);
        f.worker.run_cleanup(T0 + 400 * DAY).await;
        assert_eq!(state(&f, &a), "live");
        assert_eq!(f.fake.call_count(), 0);
    }

    #[tokio::test]
    async fn mark_used_counts_recalls_without_touching_rev() {
        let f = Fixture::new();
        let sid = f.session("a", "chat", T0);
        turn_status(&f, &sid, "q", "a", "ok", T0);
        let rev: i64 = f.q("SELECT rev FROM sessions WHERE id=?", [&sid]);
        let ids = vec![sid.clone(), "unknown".to_string()];
        mark_used(&f.lock(), &ids, T0 + 5, true).unwrap();
        mark_used(&f.lock(), &ids, T0 + 9, false).unwrap();
        let (n, used, rev2): (i64, i64, i64) = f.q2(
            "SELECT recall_count, last_used_at, rev FROM sessions WHERE id=?",
            &sid,
        );
        assert_eq!((n, used, rev2), (1, T0 + 9, rev));
    }

    /// A summarized session, `age` ms old, no use.
    async fn summarized(f: &Fixture, device: &str, at: i64) -> (String, String) {
        let sid = f.session(device, "chat", at);
        let mut p = store::tests::ap(&sid, "plan trip", "ok sure");
        p.goal_id = Some(format!("g-{device}"));
        p.tool_trace =
            serde_json::json!([{"tool": "weather", "args": {"c": "x"}, "result": "sun", "ms": 3}]);
        let t = store::append(&mut f.lock(), &f.cfg, p, at).unwrap()["turn_id"]
            .as_str()
            .unwrap()
            .to_string();
        f.lock()
            .execute(
                "UPDATE sessions SET title='Trip', summary='planned a trip', summarized_rev=rev WHERE id=?",
                [&sid],
            )
            .unwrap();
        (sid, t)
    }

    const OLD: i64 = 130 * DAY; // beyond 90 d

    #[tokio::test]
    async fn old_unused_session_is_compacted() {
        let f = Fixture::new();
        let (sid, t) = summarized(&f, "a", T0).await;
        f.worker.run_work(T0 + 10).await; // turn + summary vectors exist
        assert_eq!(f.fake.doc_ids("chats-turns"), vec![t.clone()]);
        assert_eq!(f.fake.doc_ids("chats-sessions"), vec![sid.clone()]);
        let rev0: i64 = f.q("SELECT rev FROM sessions WHERE id=?", [&sid]);
        f.worker.run_cleanup(T0 + OLD).await;
        let (tier, rev): (String, i64) = f.q2("SELECT tier, rev FROM sessions WHERE id=?", &sid);
        let (trace, trev): (String, i64) =
            f.q2("SELECT tool_trace, rev FROM turns WHERE session_id=?", &sid);
        assert_eq!((tier.as_str(), trace.as_str()), ("compact", "[]"));
        assert!(rev > rev0 && trev == rev, "session and turn got a new rev");
        assert!(
            f.fake.doc_ids("chats-turns").is_empty(),
            "turn vectors removed"
        );
        assert_eq!(
            f.fake.doc_ids("chats-sessions"),
            vec![sid.clone()],
            "summary vector stays"
        );
        assert_eq!(
            f.fake.calls("goal_delete"),
            vec![serde_json::json!({"id": "g-a"})]
        );
        assert!(
            f.fake.calls("memory_forget_session").is_empty(),
            "facts stay"
        );
        let row: i64 = f.q(
            "SELECT count(*) FROM sessions WHERE id=? AND state='live'",
            [&sid],
        );
        let queue: i64 = f.q("SELECT count(*) FROM cascade", []);
        assert_eq!((row, queue), (1, 0), "session stays, job done");
        // summary, tags and the turn text are intact
        let (summary, user): (String, String) = f.q2(
            "SELECT s.summary, t.user_text FROM sessions s JOIN turns t ON t.session_id = s.id WHERE s.id=?",
            &sid,
        );
        assert_eq!(
            (summary.as_str(), user.as_str()),
            ("planned a trip", "plan trip")
        );
        // not compacted twice
        let calls = f.fake.call_count();
        f.worker.run_cleanup(T0 + OLD + 4 * 3_600_000).await;
        assert_eq!(f.fake.call_count(), calls);
    }

    #[tokio::test]
    async fn recently_used_session_is_not_compacted() {
        let f = Fixture::new();
        let (used, _) = summarized(&f, "a", T0).await;
        let (idle, _) = summarized(&f, "b", T0).await;
        // surfaced 10 days ago -> within the 60 d "unused" window
        mark_used(
            &f.lock(),
            std::slice::from_ref(&used),
            T0 + OLD - 10 * DAY,
            true,
        )
        .unwrap();
        f.worker.run_cleanup(T0 + OLD).await;
        let tier = |id: &str| -> String { f.q("SELECT tier FROM sessions WHERE id=?", [id]) };
        assert_eq!(tier(&used), "full");
        assert_eq!(tier(&idle), "compact");
    }

    #[tokio::test]
    async fn young_pinned_and_unsummarized_sessions_wait() {
        let f = Fixture::new();
        let (young, _) = summarized(&f, "y", T0 + OLD - 10 * DAY).await;
        let (pinned, _) = summarized(&f, "p", T0).await;
        store::update(&mut f.lock(), &pinned, None, Some(true)).unwrap();
        let (nosum, _) = summarized(&f, "n", T0).await;
        f.lock()
            .execute(
                "UPDATE sessions SET summary='', summarized_rev=0 WHERE id=?",
                [&nosum],
            )
            .unwrap();
        // an ok turn keeps all three from the junk rule
        f.worker.run_cleanup(T0 + OLD).await;
        for id in [&young, &pinned, &nosum] {
            let tier: String = f.q("SELECT tier FROM sessions WHERE id=?", [id]);
            assert_eq!(tier, "full");
        }
        // once it has a summary it is compacted
        f.lock()
            .execute(
                "UPDATE sessions SET summary='s', summarized_rev=1 WHERE id=?",
                [&nosum],
            )
            .unwrap();
        f.worker.run_cleanup(T0 + OLD + 4 * 3_600_000).await;
        let tier: String = f.q("SELECT tier FROM sessions WHERE id=?", [&nosum]);
        assert_eq!(tier, "compact");
    }

    #[tokio::test]
    async fn compact_retries_goal_delete_when_agent_down() {
        let f = Fixture::new();
        let (sid, _) = summarized(&f, "a", T0).await;
        f.fake.set_failing("agent", true);
        let now = T0 + OLD;
        f.worker.run_cleanup(now).await;
        // tier and trace changed right away, the goal is still waiting
        let (tier, trace): (String, String) = f.q2(
            "SELECT s.tier, t.tool_trace FROM sessions s JOIN turns t ON t.session_id = s.id WHERE s.id=?",
            &sid,
        );
        assert_eq!((tier.as_str(), trace.as_str()), ("compact", "[]"));
        let (attempts, mode): (i64, String) = f.q2(
            "SELECT attempts, mode FROM cascade WHERE session_id=?",
            &sid,
        );
        assert_eq!((attempts, mode.as_str()), (1, "compact"));
        f.fake.set_failing("agent", false);
        f.worker.run_cleanup(now + 61_000).await;
        let queue: i64 = f.q("SELECT count(*) FROM cascade", []);
        assert_eq!(queue, 0);
        assert_eq!(
            f.fake.calls("goal_delete").len(),
            2,
            "failed once, then done"
        );
        assert!(f.fake.calls("memory_forget_session").is_empty());
    }

    #[tokio::test]
    async fn append_to_compact_restores_full() {
        let f = Fixture::new();
        let (sid, _) = summarized(&f, "a", T0).await;
        f.worker.run_cleanup(T0 + OLD).await;
        let tier: String = f.q("SELECT tier FROM sessions WHERE id=?", [&sid]);
        assert_eq!(tier, "compact");
        let since: i64 = f.q("SELECT value FROM meta WHERE key='rev'", []);
        store::append(
            &mut f.lock(),
            &f.cfg,
            store::tests::ap(&sid, "back again", "welcome"),
            T0 + OLD + 5,
        )
        .unwrap();
        let tier: String = f.q("SELECT tier FROM sessions WHERE id=?", [&sid]);
        assert_eq!(tier, "full");
        let out = crate::sync::sync(&f.lock(), since, 100, 500).unwrap();
        let s = out["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["op"] == "session")
            .unwrap();
        assert_eq!(s["session"]["tier"], "full");
        // old turns stay trimmed; the new one is intact
        let n: i64 = f.q("SELECT count(*) FROM turns WHERE session_id=?", [&sid]);
        assert_eq!(n, 2);
    }

    /// A delete arriving while a compaction job is pending takes the job over
    /// (the compaction must not finish it and leave the session half cleaned).
    #[tokio::test]
    async fn delete_takes_over_a_pending_compaction_job() {
        let f = Fixture::new();
        let (sid, _) = summarized(&f, "a", T0).await;
        f.fake.set_failing("agent", true);
        f.worker.run_cleanup(T0 + OLD).await; // compact job stuck on goal_delete
        f.delete(&sid).await;
        let (mode, gen, done): (String, i64, String) = f.q2(
            "SELECT mode, gen, done FROM cascade WHERE session_id=?",
            &sid,
        );
        assert_eq!((mode.as_str(), gen, done.as_str()), ("delete", 1, "[]"));
        f.fake.set_failing("agent", false);
        f.worker.run_cleanup(T0 + OLD + 4 * 3_600_000).await;
        assert_eq!(f.fake.calls("memory_forget_session").len(), 1);
        let rows: i64 = f.q("SELECT count(*) FROM sessions WHERE id=?", [&sid]);
        assert_eq!(rows, 0);
    }
}
