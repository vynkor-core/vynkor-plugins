//! Retention: usage tracking, automatic junk cleanup, compaction of old
//! unused sessions and the monthly owner review.
//!
//! Everything here is synchronous SQLite work. Deletions and compactions only
//! *enqueue* cleanup in the `cascade` table; the cleanup lane of the worker
//! (which runs the maintenance pass first) performs the calls to other plugins.

use rusqlite::{params, Connection, OptionalExtension};

use crate::config::Config;
use crate::db::with_db;
use crate::store;
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

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub deleted: usize,
}

impl Outcome {
    pub fn changed(&self) -> bool {
        self.deleted > 0
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
}
