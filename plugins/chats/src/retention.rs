//! Retention: usage tracking, automatic junk cleanup, compaction of old
//! unused sessions and the monthly owner review.
//!
//! Everything here is synchronous SQLite work. Deletions and compactions only
//! *enqueue* cleanup in the `cascade` table; the cleanup lane of the worker
//! (which runs the maintenance pass first) performs the calls to other plugins.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

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

// ---------------------------------------------------------------------------
// Monthly review

/// Group keys of a review.
pub const GROUP_JUNK: &str = "junk";
pub const GROUP_COMPRESS: &str = "compress";
const REVIEW_GROUP_LIMIT: i64 = 200;

/// Sessions of group `key` that qualify at `now`; `only` narrows the query to
/// one session (the re-check at apply time). Pinned and non-live sessions
/// never qualify.
fn group_sessions(
    conn: &Connection,
    key: &str,
    cfg: &Config,
    now: i64,
    only: Option<&str>,
) -> Result<Vec<String>, String> {
    let (cond, cutoff) = match key {
        // low-value voice sessions: at most one useful exchange, never recalled
        GROUP_JUNK => (
            "s.kind = 'voice' AND s.tier != 'summary' AND s.recall_count = 0
             AND s.last_at < ?1
             AND (SELECT count(*) FROM turns t WHERE t.session_id = s.id AND t.status = 'ok') <= 1",
            now - cfg.review_lowvalue_age_secs.saturating_mul(1000),
        ),
        // compact sessions nobody has looked at for a long time
        GROUP_COMPRESS => (
            "s.tier = 'compact' AND MAX(s.last_used_at, s.last_at) < ?1",
            now - cfg.review_summary_age_secs.saturating_mul(1000),
        ),
        other => return Err(format!("unknown group {other}")),
    };
    conn.prepare(&format!(
        "SELECT s.id FROM sessions s
         WHERE s.state = 'live' AND s.pinned = 0 AND {cond} AND (?2 IS NULL OR s.id = ?2)
         ORDER BY s.last_at LIMIT ?3"
    ))
    .map_err(db_err)?
    .query_map(params![cutoff, only, REVIEW_GROUP_LIMIT], |r| r.get(0))
    .map_err(db_err)?
    .collect::<rusqlite::Result<_>>()
    .map_err(db_err)
}

fn session_card(conn: &Connection, id: &str) -> Result<Value, String> {
    conn.query_row(
        "SELECT id, title, last_at, turns, tier FROM sessions WHERE id = ?",
        [id],
        |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "title": r.get::<_, String>(1)?,
                "last_at": r.get::<_, i64>(2)?,
                "turns": r.get::<_, i64>(3)?,
                "tier": r.get::<_, String>(4)?,
            }))
        },
    )
    .map_err(db_err)
}

/// Prepare the monthly review if one is due (`meta.last_review`). Without
/// candidates nothing is created; a still-pending older review is replaced
/// (and marked `dismissed`). Returns true if a review was created.
pub fn prepare_review(conn: &mut Connection, cfg: &Config, now: i64) -> Result<bool, String> {
    if meta_get(conn, "last_review")?
        .is_some_and(|l| now - l < cfg.review_interval_secs.saturating_mul(1000))
    {
        return Ok(false);
    }
    meta_set(conn, "last_review", now)?;
    let mut groups = Vec::new();
    for (key, action) in [(GROUP_JUNK, "delete"), (GROUP_COMPRESS, "summary")] {
        let ids = group_sessions(conn, key, cfg, now, None)?;
        if ids.is_empty() {
            continue;
        }
        let sessions = ids
            .iter()
            .map(|id| session_card(conn, id))
            .collect::<Result<Vec<_>, _>>()?;
        groups.push(json!({"key": key, "action": action, "sessions": sessions}));
    }
    if groups.is_empty() {
        return Ok(false);
    }
    let tx = conn.transaction().map_err(db_err)?;
    tx.execute(
        "UPDATE reviews SET status = 'dismissed' WHERE status = 'pending'",
        [],
    )
    .map_err(db_err)?;
    tx.execute(
        "INSERT INTO reviews(id, created_at, status, groups) VALUES (?, ?, 'pending', ?)",
        params![
            ulid::Ulid::new().to_string(),
            now,
            json!(groups).to_string()
        ],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(true)
}

/// `chat_review_get`: the pending review, if any.
pub fn review_get(conn: &Connection) -> Result<Value, String> {
    let row: Option<(String, i64, String)> = conn
        .query_row(
            "SELECT id, created_at, groups FROM reviews WHERE status = 'pending'
             ORDER BY created_at DESC, id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(db_err)?;
    Ok(match row {
        None => json!({"review": null}),
        Some((id, created_at, groups)) => json!({"review": {
            "id": id,
            "created_at": created_at,
            "groups": serde_json::from_str::<Value>(&groups).unwrap_or_else(|_| json!([])),
        }}),
    })
}

/// `compact -> summary`: drop the turns of the session (FTS rows go with them
/// by trigger); the session keeps its summary, tags and summary vector. No
/// tombstones: devices drop their local turns when they see `tier = summary`.
/// Returns false if the session no longer is a live, unpinned `compact` one.
pub fn summarize_tier(conn: &mut Connection, session_id: &str) -> Result<bool, String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let ok: bool = tx
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ? AND state = 'live' AND pinned = 0 AND tier = 'compact'",
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
    let rev = next_rev(&tx).map_err(db_err)?;
    tx.execute("DELETE FROM turns WHERE session_id = ?", [session_id])
        .map_err(db_err)?;
    tx.execute(
        "UPDATE sessions SET tier = 'summary', rev = ?1 WHERE id = ?2",
        params![rev, session_id],
    )
    .map_err(db_err)?;
    // belt and braces: make sure no turn vector survives (a no-op when the
    // compaction already removed them; skipped if a job is still pending)
    tx.execute(
        "INSERT OR IGNORE INTO cascade(session_id, turn_ids, goal_ids, attempts, next_at, done, mode)
         VALUES (?1, ?2, '[]', 0, 0, '[]', ?3)",
        params![session_id, json!(turn_ids).to_string(), MODE_COMPACT],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(true)
}

/// `chat_review_apply`: carry out the accepted groups of the pending review
/// `id`. Each session is re-checked first (it may have been pinned, used or
/// deleted since the review was made) and counted in `skipped` if it no longer
/// qualifies. Groups not in `accept` and sessions in `exclude` are untouched.
pub fn review_apply(
    conn: &mut Connection,
    cfg: &Config,
    id: &str,
    accept: &[String],
    exclude: &[String],
    now: i64,
) -> Result<Value, String> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT status, groups FROM reviews WHERE id = ?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(db_err)?;
    let Some((status, groups)) = row else {
        return Err("review not found".into());
    };
    if status != "pending" {
        return Err("review already applied".into());
    }
    let groups: Vec<Value> = serde_json::from_str(&groups).unwrap_or_default();
    for key in accept {
        if !groups.iter().any(|g| g["key"] == key.as_str()) {
            return Err(format!("unknown group {key} in review {id}"));
        }
    }
    let (mut deleted, mut compressed, mut skipped) = (0, 0, 0);
    for g in &groups {
        let key = g["key"].as_str().unwrap_or("");
        if !accept.iter().any(|a| a == key) {
            continue;
        }
        for sid in g["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["id"].as_str())
        {
            if exclude.iter().any(|e| e == sid) {
                continue;
            }
            // re-check: still live, unpinned and still matching the rule
            if group_sessions(conn, key, cfg, now, Some(sid))?.is_empty() {
                skipped += 1;
                continue;
            }
            let done = match key {
                GROUP_JUNK => store::delete(conn, sid)?["deleted"] == true,
                GROUP_COMPRESS => summarize_tier(conn, sid)?,
                _ => false,
            };
            match (done, key) {
                (true, GROUP_JUNK) => deleted += 1,
                (true, _) => compressed += 1,
                (false, _) => skipped += 1,
            }
        }
    }
    conn.execute("UPDATE reviews SET status = 'applied' WHERE id = ?", [id])
        .map_err(db_err)?;
    let rev: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = 'rev'", [], |r| r.get(0))
        .map_err(db_err)?;
    Ok(json!({"deleted": deleted, "compressed": compressed, "skipped": skipped, "rev": rev}))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub deleted: usize,
    pub compacted: usize,
    /// A new monthly review was prepared.
    pub review: bool,
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
    out.review = prepare_review(conn, cfg, now)?;
    Ok(out)
}

impl Worker {
    /// Run the maintenance pass; true if synced state changed. The cleanup the
    /// pass enqueues is processed by the cascade pass that follows it.
    pub(crate) async fn maintenance_pass(&self, now: i64) -> bool {
        let cfg = std::sync::Arc::clone(&self.cfg);
        match with_db(&self.db, move |c| maintenance(c, &cfg, now)).await {
            Ok(Ok(o)) => {
                if o.review {
                    (self.changed)("review");
                }
                o.changed()
            }
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

    // ----- monthly review -------------------------------------------------

    const REVIEW_NOW: i64 = T0 + 200 * DAY;

    /// V1 low-value voice (reviewable), V2 voice with two useful turns, V3 voice
    /// that was recalled, C1 old compact session, C2 recently used compact one.
    struct ReviewSet {
        v1: String,
        v2: String,
        v3: String,
        c1: String,
        c2: String,
    }

    async fn review_set(f: &Fixture) -> ReviewSet {
        let v1 = f.session("v1", "voice", T0);
        turn_status(f, &v1, "turn on the light", "done", "ok", T0);
        let v2 = f.session("v2", "voice", T0);
        turn_status(f, &v2, "q1", "a1", "ok", T0);
        turn_status(f, &v2, "q2", "a2", "ok", T0 + 1);
        let v3 = f.session("v3", "voice", T0);
        turn_status(f, &v3, "q", "a", "ok", T0);
        mark_used(&f.lock(), std::slice::from_ref(&v3), T0 + 5, true).unwrap();
        let compact = |dev: &str, used: i64| {
            let sid = f.session(dev, "chat", T0);
            turn_status(f, &sid, "plan", "trip", "ok", T0);
            f.lock()
                .execute(
                    "UPDATE sessions SET tier='compact', title='Trip', summary='a trip', summarized_rev=rev, last_used_at=?1 WHERE id=?2",
                    params![used, sid],
                )
                .unwrap();
            sid
        };
        let c1 = compact("c1", 0);
        let c2 = compact("c2", REVIEW_NOW - 10 * DAY);
        ReviewSet { v1, v2, v3, c1, c2 }
    }

    fn make_review(f: &Fixture) -> Value {
        assert!(prepare_review(&mut f.lock(), &f.cfg, REVIEW_NOW).unwrap());
        review_get(&f.lock()).unwrap()["review"].clone()
    }

    fn group_ids(review: &Value, key: &str) -> Vec<String> {
        review["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["key"] == key)
            .map(|g| {
                g["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn monthly_review_builds_groups() {
        let f = Fixture::new();
        let set = review_set(&f).await;
        let r = make_review(&f);
        assert_eq!(group_ids(&r, "junk"), vec![set.v1.clone()]);
        assert_eq!(group_ids(&r, "compress"), vec![set.c1.clone()]);
        let g = &r["groups"][0];
        assert_eq!(
            (g["key"].as_str(), g["action"].as_str()),
            (Some("junk"), Some("delete"))
        );
        assert_eq!(g["sessions"][0]["tier"], "full");
        assert_eq!(r["groups"][1]["action"], "summary");
        for not_listed in [&set.v2, &set.v3, &set.c2] {
            assert!(!r.to_string().contains(not_listed.as_str()));
        }
        // not due again before the interval
        assert!(!prepare_review(&mut f.lock(), &f.cfg, REVIEW_NOW + DAY).unwrap());
        // a newer review replaces a pending one
        let first = r["id"].as_str().unwrap().to_string();
        assert!(prepare_review(&mut f.lock(), &f.cfg, REVIEW_NOW + 31 * DAY).unwrap());
        let second = review_get(&f.lock()).unwrap()["review"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(first, second);
        let st: String = f.q("SELECT status FROM reviews WHERE id=?", [&first]);
        assert_eq!(st, "dismissed");
    }

    #[tokio::test]
    async fn worker_prepares_the_review_and_announces_it() {
        let f = Fixture::new();
        review_set(&f).await;
        f.worker.run_cleanup(REVIEW_NOW).await;
        assert!(f.events.lock().unwrap().iter().any(|e| e == "review"));
        assert!(review_get(&f.lock()).unwrap()["review"].is_object());
    }

    #[tokio::test]
    async fn no_candidates_no_review() {
        let f = Fixture::new();
        let v = f.session("v", "voice", T0);
        turn_status(&f, &v, "a", "b", "ok", T0 + 1);
        turn_status(&f, &v, "c", "d", "ok", T0 + 2);
        assert!(!prepare_review(&mut f.lock(), &f.cfg, REVIEW_NOW).unwrap());
        assert_eq!(
            review_get(&f.lock()).unwrap(),
            serde_json::json!({"review": null})
        );
        let n: i64 = f.q("SELECT count(*) FROM reviews", []);
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn sync_reports_review_pending() {
        let f = Fixture::new();
        review_set(&f).await;
        let pending = |f: &Fixture| {
            crate::sync::sync(&f.lock(), 0, 100, 500).unwrap()["review_pending"].clone()
        };
        assert_eq!(pending(&f), false);
        let r = make_review(&f);
        assert_eq!(pending(&f), true);
        let since = crate::sync::sync(&f.lock(), 0, 1000, 1000).unwrap()["rev"]
            .as_i64()
            .unwrap();
        assert_eq!(
            crate::sync::sync(&f.lock(), since, 100, 500).unwrap()["review_pending"],
            true
        );
        review_apply(
            &mut f.lock(),
            &f.cfg,
            r["id"].as_str().unwrap(),
            &["junk".to_string()],
            &[],
            REVIEW_NOW,
        )
        .unwrap();
        assert_eq!(pending(&f), false);
    }

    #[tokio::test]
    async fn apply_deletes_and_compresses_accepted_groups_only() {
        let f = Fixture::new();
        let set = review_set(&f).await;
        let r = make_review(&f);
        let id = r["id"].as_str().unwrap();
        let out = review_apply(
            &mut f.lock(),
            &f.cfg,
            id,
            &["junk".to_string()],
            &[],
            REVIEW_NOW,
        )
        .unwrap();
        assert_eq!(
            (
                out["deleted"].as_i64(),
                out["compressed"].as_i64(),
                out["skipped"].as_i64()
            ),
            (Some(1), Some(0), Some(0))
        );
        assert_eq!(state(&f, &set.v1), "deleting");
        let tier: String = f.q("SELECT tier FROM sessions WHERE id=?", [&set.c1]);
        assert_eq!(tier, "compact", "compress group was not accepted");
        let st: String = f.q("SELECT status FROM reviews WHERE id=?", [id]);
        assert_eq!(st, "applied");
        // the deletion goes through the cascade
        f.worker.run_cleanup(REVIEW_NOW + 1).await;
        let rows: i64 = f.q("SELECT count(*) FROM sessions WHERE id=?", [&set.v1]);
        assert_eq!(rows, 0);
        assert!(request_changed("chat_review_apply", &out));
    }

    #[tokio::test]
    async fn apply_both_groups_and_exclude_a_session() {
        let f = Fixture::new();
        let set = review_set(&f).await;
        let r = make_review(&f);
        let out = review_apply(
            &mut f.lock(),
            &f.cfg,
            r["id"].as_str().unwrap(),
            &["junk".to_string(), "compress".to_string()],
            std::slice::from_ref(&set.v1),
            REVIEW_NOW,
        )
        .unwrap();
        assert_eq!(
            (out["deleted"].as_i64(), out["compressed"].as_i64()),
            (Some(0), Some(1))
        );
        assert_eq!(state(&f, &set.v1), "live", "excluded by the owner");
        let tier: String = f.q("SELECT tier FROM sessions WHERE id=?", [&set.c1]);
        assert_eq!(tier, "summary");
    }

    #[tokio::test]
    async fn apply_skips_pinned_since_review() {
        let f = Fixture::new();
        let set = review_set(&f).await;
        let r = make_review(&f);
        store::update(&mut f.lock(), &set.v1, None, Some(true)).unwrap();
        store::update(&mut f.lock(), &set.c1, None, Some(true)).unwrap();
        let out = review_apply(
            &mut f.lock(),
            &f.cfg,
            r["id"].as_str().unwrap(),
            &["junk".to_string(), "compress".to_string()],
            &[],
            REVIEW_NOW,
        )
        .unwrap();
        assert_eq!(
            (
                out["deleted"].as_i64(),
                out["compressed"].as_i64(),
                out["skipped"].as_i64()
            ),
            (Some(0), Some(0), Some(2))
        );
        assert_eq!(state(&f, &set.v1), "live");
        let (tier, n): (String, i64) = f.q2(
            "SELECT s.tier, (SELECT count(*) FROM turns t WHERE t.session_id = s.id) FROM sessions s WHERE s.id=?",
            &set.c1,
        );
        assert_eq!((tier.as_str(), n), ("compact", 1));
        assert!(!request_changed("chat_review_apply", &out));
    }

    #[tokio::test]
    async fn apply_skips_sessions_used_since_review() {
        let f = Fixture::new();
        let set = review_set(&f).await;
        let r = make_review(&f);
        mark_used(
            &f.lock(),
            std::slice::from_ref(&set.c1),
            REVIEW_NOW - DAY,
            true,
        )
        .unwrap();
        mark_used(
            &f.lock(),
            std::slice::from_ref(&set.v1),
            REVIEW_NOW - DAY,
            true,
        )
        .unwrap();
        let out = review_apply(
            &mut f.lock(),
            &f.cfg,
            r["id"].as_str().unwrap(),
            &["junk".to_string(), "compress".to_string()],
            &[],
            REVIEW_NOW,
        )
        .unwrap();
        assert_eq!(out["skipped"], 2);
    }

    #[tokio::test]
    async fn apply_rejects_unknown_and_repeated() {
        let f = Fixture::new();
        let set = review_set(&f).await;
        let r = make_review(&f);
        let id = r["id"].as_str().unwrap().to_string();
        let junk = vec!["junk".to_string()];
        let e = review_apply(&mut f.lock(), &f.cfg, "nope", &junk, &[], REVIEW_NOW).unwrap_err();
        assert_eq!(e, "review not found");
        let e = review_apply(
            &mut f.lock(),
            &f.cfg,
            &id,
            &["bogus".to_string()],
            &[],
            REVIEW_NOW,
        )
        .unwrap_err();
        assert!(e.contains("unknown group"), "{e}");
        assert_eq!(state(&f, &set.v1), "live", "errors change nothing");
        review_apply(&mut f.lock(), &f.cfg, &id, &junk, &[], REVIEW_NOW).unwrap();
        let rev: i64 = f.q("SELECT value FROM meta WHERE key='rev'", []);
        let e = review_apply(&mut f.lock(), &f.cfg, &id, &junk, &[], REVIEW_NOW).unwrap_err();
        assert_eq!(e, "review already applied");
        assert_eq!(
            f.q::<i64>("SELECT value FROM meta WHERE key='rev'", []),
            rev
        );
    }

    fn request_changed(action: &str, result: &Value) -> bool {
        crate::request::is_mutation(action, result)
    }

    #[tokio::test]
    async fn summary_tier_drops_turns_and_fts() {
        let f = Fixture::new();
        f.fake.synonym("forecast", "weather");
        let sid = f.session("c", "chat", T0);
        turn_status(&f, &sid, "zebra stripes", "african animal", "ok", T0);
        f.lock()
            .execute(
                "UPDATE sessions SET tier='compact', title='Trip', summary='forecast', summarized_rev=rev WHERE id=?",
                [&sid],
            )
            .unwrap();
        f.worker.run_work(T0 + 10).await; // turn + summary vectors
        let since: i64 = f.q("SELECT value FROM meta WHERE key='rev'", []);
        let r = make_review(&f);
        review_apply(
            &mut f.lock(),
            &f.cfg,
            r["id"].as_str().unwrap(),
            &["compress".to_string()],
            &[],
            REVIEW_NOW,
        )
        .unwrap();
        f.worker.run_cleanup(REVIEW_NOW + 1).await;
        // the turn text is gone from search
        let sp = |q: &str| crate::search::SearchParams {
            query: q.into(),
            ..Default::default()
        };
        assert!(
            crate::search::search(&f.lock(), &sp("zebra")).unwrap()["hits"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            f.fake.doc_ids("chats-turns").is_empty(),
            "turn vectors removed"
        );
        // the summary is still found (vector hit on the session)
        let svc = crate::service::Service::new(
            std::sync::Arc::clone(&f.db),
            f.rpc.clone(),
            std::sync::Arc::clone(&f.cfg),
        );
        let hits = svc
            .handle("chat_search", br#"{"query":"weather"}"#, REVIEW_NOW + 2)
            .await
            .unwrap();
        assert_eq!(hits["hits"][0]["session_id"], sid.as_str());
        assert!(hits["hits"][0]["turn_id"].is_null());
        // sync: the session carries tier=summary, no turn tombstones exist
        let out = crate::sync::sync(&f.lock(), since, 100, 500).unwrap();
        let ch = out["changes"].as_array().unwrap();
        assert!(ch
            .iter()
            .any(|c| c["op"] == "session" && c["session"]["tier"] == "summary"));
        assert!(ch.iter().all(|c| c["op"] != "delete"));
        let n: i64 = f.q("SELECT count(*) FROM turns WHERE session_id=?", [&sid]);
        assert_eq!(n, 0);
    }
}
