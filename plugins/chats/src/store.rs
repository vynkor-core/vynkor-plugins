use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;

#[derive(Debug, Deserialize)]
pub struct ResolveParams {
    pub device: String,
    pub kind: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppendParams {
    pub session_id: String,
    #[serde(default)]
    pub user_text: String,
    #[serde(default)]
    pub answer_text: String,
    pub status: String,
    #[serde(default)]
    pub tool_trace: Value,
    #[serde(default)]
    pub goal_id: Option<String>,
}

pub fn next_rev(tx: &Transaction) -> rusqlite::Result<i64> {
    tx.query_row(
        "UPDATE meta SET value = value + 1 WHERE key = 'rev' RETURNING value",
        [],
        |r| r.get(0),
    )
}

pub const KINDS: [&str; 2] = ["chat", "voice"];
pub const STATUSES: [&str; 3] = ["ok", "failed", "cancelled"];
const MAX_TRACE_ENTRIES: usize = 50;

fn db_err(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

/// Cut `s` to at most `max` bytes on a char boundary; a cut string ends with `…`.
pub fn truncate_bytes(s: &str, max: usize) -> String {
    const ELLIPSIS: &str = "…";
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub(ELLIPSIS.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &s[..end])
}

/// Cut `s` to at most `max` chars; a cut string ends with `…`.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn clip_field(v: &Value, max: usize) -> Value {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Value::String(truncate_chars(&s, max))
}

fn normalize_trace(trace: &Value, cfg: &Config) -> Value {
    let Value::Array(items) = trace else {
        return json!([]);
    };
    let out: Vec<Value> = items
        .iter()
        .take(MAX_TRACE_ENTRIES)
        .map(|item| {
            let Value::Object(map) = item else {
                return item.clone();
            };
            let mut map = map.clone();
            for k in ["args", "result", "error"] {
                if let Some(v) = map.get(k) {
                    if !v.is_null() {
                        let clipped = clip_field(v, cfg.trace_max_chars);
                        map.insert(k.to_string(), clipped);
                    }
                }
            }
            Value::Object(map)
        })
        .collect();
    Value::Array(out)
}

fn new_session(tx: &Transaction, p: &ResolveParams, now: i64) -> Result<String, String> {
    let rev = next_rev(tx).map_err(db_err)?;
    let id = ulid::Ulid::new().to_string();
    tx.execute(
        "INSERT INTO sessions(id, kind, device, source, started_at, last_at, rev) VALUES (?,?,?,?,?,?,?)",
        params![id, p.kind, p.device, p.source, now, now, rev],
    )
    .map_err(db_err)?;
    Ok(id)
}

/// Pick the session a new turn belongs to (see overview §2 for the rules).
pub fn resolve(
    conn: &mut Connection,
    cfg: &Config,
    p: &ResolveParams,
    now: i64,
) -> Result<Value, String> {
    if !KINDS.contains(&p.kind.as_str()) {
        return Err(format!("kind must be chat or voice, got {:?}", p.kind));
    }
    if p.device.trim().is_empty() {
        return Err("device is required".into());
    }
    let pause_ms = cfg.pause_secs.saturating_mul(1000);
    let fresh = |last_at: i64| p.kind == "chat" || now - last_at < pause_ms;
    let tx = conn.transaction().map_err(db_err)?;
    let mut found: Option<String> = None;
    if let Some(id) = &p.session_id {
        let row: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT device, kind, last_at FROM sessions WHERE id = ? AND state = 'live'",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(db_err)?;
        if let Some((device, kind, last_at)) = row {
            if device == p.device && kind == p.kind && fresh(last_at) {
                found = Some(id.clone());
            }
        }
    } else {
        let row: Option<(String, i64)> = tx
            .query_row(
                "SELECT id, last_at FROM sessions WHERE device = ? AND kind = ? AND state = 'live'
                 ORDER BY last_at DESC, id DESC LIMIT 1",
                [&p.device, &p.kind],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_err)?;
        if let Some((id, last_at)) = row {
            if now - last_at < pause_ms {
                found = Some(id);
            }
        }
    }
    let (session_id, created) = match found {
        Some(id) => (id, false),
        None => (new_session(&tx, p, now)?, true),
    };
    tx.commit().map_err(db_err)?;
    Ok(json!({"session_id": session_id, "created": created}))
}

/// Add a turn to a live session; one transaction, one revision.
pub fn append(
    conn: &mut Connection,
    cfg: &Config,
    p: AppendParams,
    now: i64,
) -> Result<Value, String> {
    if !STATUSES.contains(&p.status.as_str()) {
        return Err(format!(
            "status must be ok, failed or cancelled, got {:?}",
            p.status
        ));
    }
    let user_text = truncate_bytes(&p.user_text, cfg.text_max_bytes);
    let answer_text = truncate_bytes(&p.answer_text, cfg.text_max_bytes);
    let trace = normalize_trace(&p.tool_trace, cfg).to_string();
    let tx = conn.transaction().map_err(db_err)?;
    let count: Option<i64> = tx
        .query_row(
            "SELECT turns FROM sessions WHERE id = ? AND state = 'live'",
            [&p.session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let Some(count) = count else {
        return Err(format!("unknown session {}", p.session_id));
    };
    let rev = next_rev(&tx).map_err(db_err)?;
    let turn_id = ulid::Ulid::new().to_string();
    let seq = count + 1;
    tx.execute(
        "INSERT INTO turns(id, session_id, seq, at, user_text, answer_text, status, tool_trace, goal_id, rev)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![turn_id, p.session_id, seq, now, user_text, answer_text, p.status, trace, p.goal_id, rev],
    )
    .map_err(db_err)?;
    tx.execute(
        "UPDATE sessions SET turns = ?, last_at = MAX(last_at, ?), rev = ? WHERE id = ?",
        params![seq, now, rev, p.session_id],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(json!({"turn_id": turn_id, "seq": seq, "rev": rev}))
}

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub kind: Option<String>,
    pub source: Option<String>,
    pub limit: Option<i64>,
    pub before: Option<i64>,
}

/// Column list matching [`session_from_row`].
pub const SESSION_COLS: &str =
    "id, kind, device, source, title, started_at, last_at, summary, tags, tier, pinned, turns";
/// Column list matching [`turn_from_row`] (columns 0..=7).
pub const TURN_COLS: &str = "id, seq, at, user_text, answer_text, status, tool_trace, goal_id";

fn parse_json_or(s: &str, fallback: Value) -> Value {
    serde_json::from_str(s).unwrap_or(fallback)
}

pub fn session_from_row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let tags: String = r.get(8)?;
    Ok(json!({
        "id": r.get::<_, String>(0)?,
        "kind": r.get::<_, String>(1)?,
        "device": r.get::<_, String>(2)?,
        "source": r.get::<_, String>(3)?,
        "title": r.get::<_, String>(4)?,
        "started_at": r.get::<_, i64>(5)?,
        "last_at": r.get::<_, i64>(6)?,
        "summary": r.get::<_, String>(7)?,
        "tags": parse_json_or(&tags, json!([])),
        "tier": r.get::<_, String>(9)?,
        "pinned": r.get::<_, i64>(10)? != 0,
        "turns": r.get::<_, i64>(11)?,
    }))
}

pub fn turn_from_row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let trace: String = r.get(6)?;
    Ok(json!({
        "id": r.get::<_, String>(0)?,
        "seq": r.get::<_, i64>(1)?,
        "at": r.get::<_, i64>(2)?,
        "user_text": r.get::<_, String>(3)?,
        "answer_text": r.get::<_, String>(4)?,
        "status": r.get::<_, String>(5)?,
        "tool_trace": parse_json_or(&trace, json!([])),
        "goal_id": r.get::<_, Option<String>>(7)?,
    }))
}

/// Live sessions only, most recently active first.
pub fn list(conn: &Connection, p: &ListParams) -> Result<Value, String> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let sql = format!(
        "SELECT {SESSION_COLS} FROM sessions
         WHERE state = 'live'
           AND (?1 IS NULL OR last_at >= ?1)
           AND (?2 IS NULL OR last_at <= ?2)
           AND (?3 IS NULL OR kind = ?3)
           AND (?4 IS NULL OR source = ?4)
           AND (?5 IS NULL OR last_at < ?5)
         ORDER BY last_at DESC, id DESC LIMIT ?6"
    );
    let mut stmt = conn.prepare_cached(&sql).map_err(db_err)?;
    let rows = stmt
        .query_map(
            params![p.from, p.to, p.kind, p.source, p.before, limit],
            session_from_row,
        )
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?;
    Ok(json!({"sessions": rows}))
}

pub fn read(conn: &Connection, id: &str, last: Option<u32>) -> Result<Value, String> {
    let session = conn
        .query_row(
            &format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ? AND state = 'live'"),
            [id],
            session_from_row,
        )
        .optional()
        .map_err(db_err)?
        .ok_or_else(|| format!("unknown session {id}"))?;
    let sql = match last {
        Some(_) => format!(
            "SELECT {TURN_COLS} FROM turns WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2"
        ),
        None => {
            format!("SELECT {TURN_COLS} FROM turns WHERE session_id = ?1 ORDER BY seq ASC LIMIT ?2")
        }
    };
    let mut stmt = conn.prepare_cached(&sql).map_err(db_err)?;
    let mut turns = stmt
        .query_map(
            params![id, last.map(i64::from).unwrap_or(-1)],
            turn_from_row,
        )
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?;
    if last.is_some() {
        turns.reverse();
    }
    Ok(json!({"session": session, "turns": turns}))
}

/// Rename and/or pin a session. Does not touch `last_at`.
pub fn update(
    conn: &mut Connection,
    id: &str,
    title: Option<String>,
    pinned: Option<bool>,
) -> Result<Value, String> {
    if title.is_none() && pinned.is_none() {
        return Err("nothing to update: pass title and/or pinned".into());
    }
    let title = title.map(|t| truncate_chars(t.trim(), 512));
    let tx = conn.transaction().map_err(db_err)?;
    let exists: bool = tx
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ? AND state = 'live'",
            [id],
            |_| Ok(true),
        )
        .optional()
        .map_err(db_err)?
        .unwrap_or(false);
    if !exists {
        return Err(format!("unknown session {id}"));
    }
    let rev = next_rev(&tx).map_err(db_err)?;
    tx.execute(
        "UPDATE sessions SET title = COALESCE(?1, title), pinned = COALESCE(?2, pinned), rev = ?3 WHERE id = ?4",
        params![title, pinned.map(i64::from), rev, id],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(json!({"rev": rev}))
}

/// Hide a session everywhere at once: one transaction marks it `deleting`,
/// drops its turns (and their FTS rows) and records a tombstone. Idempotent.
/// Removing the session row itself and cascading to other plugins is later work.
pub fn delete(conn: &mut Connection, id: &str) -> Result<Value, String> {
    let tx = conn.transaction().map_err(db_err)?;
    let existing: Option<i64> = tx
        .query_row("SELECT rev FROM tombstones WHERE id = ?", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(db_err)?;
    if let Some(rev) = existing {
        return Ok(json!({"rev": rev}));
    }
    let live: bool = tx
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ? AND state = 'live'",
            [id],
            |_| Ok(true),
        )
        .optional()
        .map_err(db_err)?
        .unwrap_or(false);
    if !live {
        return Err(format!("unknown session {id}"));
    }
    let rev = next_rev(&tx).map_err(db_err)?;
    tx.execute(
        "UPDATE sessions SET state = 'deleting', rev = ? WHERE id = ?",
        params![rev, id],
    )
    .map_err(db_err)?;
    tx.execute("DELETE FROM turns WHERE session_id = ?", [id])
        .map_err(db_err)?;
    tx.execute(
        "INSERT INTO tombstones(id, rev) VALUES (?, ?)",
        params![id, rev],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(json!({"rev": rev}))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::schema::open_temp;

    pub fn rp(device: &str, kind: &str, sid: Option<&str>) -> ResolveParams {
        ResolveParams {
            device: device.into(),
            kind: kind.into(),
            source: "test".into(),
            session_id: sid.map(String::from),
        }
    }

    pub fn ap(sid: &str, user: &str, answer: &str) -> AppendParams {
        AppendParams {
            session_id: sid.into(),
            user_text: user.into(),
            answer_text: answer.into(),
            status: "ok".into(),
            tool_trace: Value::Null,
            goal_id: None,
        }
    }

    pub fn sid(v: &Value) -> String {
        v["session_id"].as_str().unwrap().to_string()
    }

    const T0: i64 = 1_000_000_000;
    const PAUSE_MS: i64 = 1_800_000;

    #[test]
    fn resolve_creates_then_reuses_within_pause() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let a = resolve(&mut c, &cfg, &rp("lap", "voice", None), T0).unwrap();
        assert_eq!(a["created"], true);
        append(&mut c, &cfg, ap(&sid(&a), "hi", "hello"), T0 + 1000).unwrap();
        let b = resolve(
            &mut c,
            &cfg,
            &rp("lap", "voice", None),
            T0 + 1000 + PAUSE_MS - 1,
        )
        .unwrap();
        assert_eq!(b["created"], false);
        assert_eq!(sid(&a), sid(&b));
        let c2 = resolve(
            &mut c,
            &cfg,
            &rp("lap", "voice", Some(&sid(&a))),
            T0 + 1000 + PAUSE_MS - 1,
        )
        .unwrap();
        assert_eq!(sid(&c2), sid(&a));
    }

    #[test]
    fn resolve_new_after_pause() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let a = resolve(&mut c, &cfg, &rp("lap", "voice", None), T0).unwrap();
        let b = resolve(&mut c, &cfg, &rp("lap", "voice", None), T0 + PAUSE_MS).unwrap();
        assert_eq!(b["created"], true);
        assert_ne!(sid(&a), sid(&b));
        // an explicit id past the pause also splits
        let d = resolve(
            &mut c,
            &cfg,
            &rp("lap", "voice", Some(&sid(&a))),
            T0 + PAUSE_MS,
        )
        .unwrap();
        assert_eq!(d["created"], true);
        assert_ne!(sid(&d), sid(&a));
    }

    #[test]
    fn resolve_separates_devices_and_kinds() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let a = resolve(&mut c, &cfg, &rp("lap", "voice", None), T0).unwrap();
        let b = resolve(&mut c, &cfg, &rp("phone", "voice", None), T0 + 1).unwrap();
        let k = resolve(&mut c, &cfg, &rp("lap", "chat", None), T0 + 2).unwrap();
        assert_eq!(b["created"], true);
        assert_eq!(k["created"], true);
        assert_ne!(sid(&a), sid(&b));
        assert_ne!(sid(&a), sid(&k));
        // explicit id of another device / kind is not adopted
        let x = resolve(&mut c, &cfg, &rp("phone", "voice", Some(&sid(&a))), T0 + 3).unwrap();
        assert_eq!(x["created"], true);
        assert_ne!(sid(&x), sid(&a));
        let y = resolve(&mut c, &cfg, &rp("lap", "chat", Some(&sid(&a))), T0 + 4).unwrap();
        assert_ne!(sid(&y), sid(&a));
    }

    #[test]
    fn resolve_chat_ignores_pause() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let a = resolve(&mut c, &cfg, &rp("lap", "chat", None), T0).unwrap();
        let b = resolve(
            &mut c,
            &cfg,
            &rp("lap", "chat", Some(&sid(&a))),
            T0 + 100 * PAUSE_MS,
        )
        .unwrap();
        assert_eq!(b["created"], false);
        assert_eq!(sid(&a), sid(&b));
        // without an id a chat still starts fresh only after the pause
        let n = resolve(&mut c, &cfg, &rp("lap", "chat", None), T0 + 100 * PAUSE_MS).unwrap();
        assert_eq!(n["created"], true);
    }

    #[test]
    fn resolve_unknown_id_creates_new() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let a = resolve(&mut c, &cfg, &rp("lap", "chat", Some("nope")), T0).unwrap();
        assert_eq!(a["created"], true);
        assert_ne!(sid(&a), "nope");
        assert!(resolve(&mut c, &cfg, &rp("lap", "bogus", None), T0).is_err());
    }

    #[test]
    fn append_assigns_seq_and_rev() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), T0).unwrap());
        let r1 = append(&mut c, &cfg, ap(&s, "q1", "a1"), T0 + 10).unwrap();
        let r2 = append(&mut c, &cfg, ap(&s, "q2", "a2"), T0 + 20).unwrap();
        assert_eq!((r1["seq"].as_i64(), r2["seq"].as_i64()), (Some(1), Some(2)));
        assert!(r2["rev"].as_i64() > r1["rev"].as_i64());
        assert_ne!(r1["turn_id"], r2["turn_id"]);
        let (turns, last_at, rev): (i64, i64, i64) = c
            .query_row(
                "SELECT turns, last_at, rev FROM sessions WHERE id=?",
                [&s],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((turns, last_at), (2, T0 + 20));
        assert_eq!(Some(rev), r2["rev"].as_i64());
        let trev: i64 = c
            .query_row("SELECT rev FROM turns WHERE seq=2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(Some(trev), r2["rev"].as_i64());
    }

    #[test]
    fn append_unknown_session_errors() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let e = append(&mut c, &cfg, ap("nope", "q", "a"), T0).unwrap_err();
        assert_eq!(e, "unknown session nope");
    }

    #[test]
    fn append_rejects_bad_status() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), T0).unwrap());
        let mut p = ap(&s, "q", "a");
        p.status = "weird".into();
        assert!(append(&mut c, &cfg, p, T0).unwrap_err().contains("status"));
        let n: i64 = c
            .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn append_truncates_long_text_and_trace() {
        let (_d, mut c) = open_temp();
        let mut cfg = test_config();
        cfg.text_max_bytes = 100;
        cfg.trace_max_chars = 10;
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), T0).unwrap());
        let mut p = ap(&s, &"ж".repeat(500), &"x".repeat(500));
        let trace: Vec<Value> = (0..60)
            .map(|i| json!({"tool":"t","args":{"big":"a".repeat(100)},"result":"r".repeat(100),"error":"e".repeat(100),"ms":i}))
            .collect();
        p.tool_trace = Value::Array(trace);
        append(&mut c, &cfg, p, T0).unwrap();
        let (u, a, t): (String, String, String) = c
            .query_row(
                "SELECT user_text, answer_text, tool_trace FROM turns",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(
            u.len() <= 100 && u.ends_with('…'),
            "user_text len {}",
            u.len()
        );
        assert!(u.is_char_boundary(u.len()));
        assert!(a.len() <= 100 && a.ends_with('…'));
        let t: Vec<Value> = serde_json::from_str(&t).unwrap();
        assert_eq!(t.len(), 50);
        for e in &t {
            for k in ["args", "result", "error"] {
                let s = e[k].as_str().unwrap();
                assert!(s.chars().count() <= 10, "{k}: {s}");
            }
            assert_eq!(e["tool"], "t");
        }
        assert!(t[0]["result"].as_str().unwrap().ends_with('…'));
    }

    #[test]
    fn short_text_untouched() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), T0).unwrap());
        let mut p = ap(&s, "привет", "ok");
        p.tool_trace = json!([{"tool":"t","args":{"a":1},"result":"fine","ms":3}]);
        append(&mut c, &cfg, p, T0).unwrap();
        let (u, t): (String, String) = c
            .query_row("SELECT user_text, tool_trace FROM turns", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(u, "привет");
        let t: Vec<Value> = serde_json::from_str(&t).unwrap();
        assert_eq!(t[0]["args"], "{\"a\":1}");
        assert_eq!(t[0]["ms"], 3);
    }

    #[test]
    fn concurrent_appends_unique_increasing_rev() {
        use std::sync::{Arc, Mutex};
        let (_d, c) = open_temp();
        let cfg = Arc::new(test_config());
        let c = Arc::new(Mutex::new(c));
        let s = {
            let mut g = c.lock().unwrap();
            sid(&resolve(&mut g, &cfg, &rp("lap", "chat", None), T0).unwrap())
        };
        // revs in commit order: pushed while still holding the writer lock
        let order = Arc::new(Mutex::new(Vec::<i64>::new()));
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let (c, cfg, order, s) = (c.clone(), cfg.clone(), order.clone(), s.clone());
                std::thread::spawn(move || {
                    for i in 0..50 {
                        let mut g = c.lock().unwrap();
                        let r = append(&mut g, &cfg, ap(&s, &format!("t{t}-{i}"), "a"), T0 + i)
                            .unwrap();
                        order.lock().unwrap().push(r["rev"].as_i64().unwrap());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let revs = order.lock().unwrap().clone();
        assert_eq!(revs.len(), 200);
        assert!(
            revs.windows(2).all(|w| w[0] < w[1]),
            "revs not strictly increasing"
        );
        let distinct: i64 = c
            .lock()
            .unwrap()
            .query_row("SELECT count(DISTINCT rev) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(distinct, 200);
    }

    fn mk(
        c: &mut Connection,
        cfg: &Config,
        device: &str,
        kind: &str,
        source: &str,
        at: i64,
    ) -> String {
        let mut p = rp(device, kind, None);
        p.source = source.into();
        let s = sid(&resolve(c, cfg, &p, at).unwrap());
        append(c, cfg, ap(&s, "q", "a"), at).unwrap();
        s
    }

    #[test]
    fn list_orders_by_last_at_and_filters() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let a = mk(&mut c, &cfg, "lap", "chat", "desktop", 100);
        let b = mk(&mut c, &cfg, "lap", "voice", "daemon", 300);
        let d = mk(&mut c, &cfg, "phone", "chat", "desktop", 200);
        let ids = |v: Value| -> Vec<String> {
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            ids(list(&c, &ListParams::default()).unwrap()),
            vec![b.clone(), d.clone(), a.clone()]
        );
        let by_kind = ListParams {
            kind: Some("chat".into()),
            ..Default::default()
        };
        assert_eq!(ids(list(&c, &by_kind).unwrap()), vec![d.clone(), a.clone()]);
        let by_src = ListParams {
            source: Some("daemon".into()),
            ..Default::default()
        };
        assert_eq!(ids(list(&c, &by_src).unwrap()), vec![b.clone()]);
        let range = ListParams {
            from: Some(150),
            to: Some(250),
            ..Default::default()
        };
        assert_eq!(ids(list(&c, &range).unwrap()), vec![d.clone()]);
        let one = list(
            &c,
            &ListParams {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let s = &one["sessions"][0];
        assert_eq!(s["id"], b.as_str());
        assert_eq!(s["turns"], 1);
        assert_eq!(s["pinned"], false);
        assert_eq!(s["tags"], json!([]));
        assert_eq!(s["tier"], "full");
        assert_eq!(s["device"], "lap");
        assert!(s.get("rev").is_none());
    }

    #[test]
    fn list_pagination_before() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let ids: Vec<String> = (1..=5)
            .map(|i| mk(&mut c, &cfg, &format!("dev{i}"), "chat", "x", i * 100))
            .collect();
        let p1 = list(
            &c,
            &ListParams {
                limit: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        let p1 = p1["sessions"].as_array().unwrap().clone();
        assert_eq!(p1[0]["id"], ids[4].as_str());
        assert_eq!(p1[1]["id"], ids[3].as_str());
        let before = p1[1]["last_at"].as_i64().unwrap();
        let p2 = list(
            &c,
            &ListParams {
                limit: Some(2),
                before: Some(before),
                ..Default::default()
            },
        )
        .unwrap();
        let p2 = p2["sessions"].as_array().unwrap().clone();
        assert_eq!(
            (p2[0]["id"].as_str(), p2[1]["id"].as_str()),
            (Some(ids[2].as_str()), Some(ids[1].as_str()))
        );
        // limit is clamped to 200 and floored at 1
        assert!(list(
            &c,
            &ListParams {
                limit: Some(100_000),
                ..Default::default()
            }
        )
        .is_ok());
        assert_eq!(
            list(
                &c,
                &ListParams {
                    limit: Some(0),
                    ..Default::default()
                }
            )
            .unwrap()["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn read_returns_turns_in_order() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), 10).unwrap());
        for i in 1..=3 {
            let mut p = ap(&s, &format!("q{i}"), &format!("a{i}"));
            p.goal_id = Some("g1".into());
            p.tool_trace = json!([{"tool":"t","args":"x","result":"y","ms":1}]);
            append(&mut c, &cfg, p, 10 + i).unwrap();
        }
        let r = read(&c, &s, None).unwrap();
        assert_eq!(r["session"]["id"], s.as_str());
        let turns = r["turns"].as_array().unwrap();
        assert_eq!(
            turns
                .iter()
                .map(|t| t["seq"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(turns[0]["user_text"], "q1");
        assert_eq!(turns[0]["answer_text"], "a1");
        assert_eq!(turns[0]["status"], "ok");
        assert_eq!(turns[0]["goal_id"], "g1");
        assert_eq!(turns[0]["tool_trace"][0]["tool"], "t");
        assert_eq!(turns[0]["at"], 11);
        assert!(turns[0]["id"].is_string());
        assert_eq!(read(&c, "nope", None).unwrap_err(), "unknown session nope");
    }

    #[test]
    fn read_last_n() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), 10).unwrap());
        for i in 1..=5 {
            append(&mut c, &cfg, ap(&s, &format!("q{i}"), "a"), 10 + i).unwrap();
        }
        let r = read(&c, &s, Some(2)).unwrap();
        let seqs: Vec<i64> = r["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["seq"].as_i64().unwrap())
            .collect();
        assert_eq!(seqs, vec![4, 5]);
    }

    #[test]
    fn update_sets_title_and_pinned_bumps_rev() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = mk(&mut c, &cfg, "lap", "chat", "x", 100);
        let before: i64 = c
            .query_row("SELECT rev FROM sessions WHERE id=?", [&s], |r| r.get(0))
            .unwrap();
        let r = update(&mut c, &s, Some("Weather chat".into()), Some(true)).unwrap();
        assert!(r["rev"].as_i64().unwrap() > before);
        let got = read(&c, &s, None).unwrap();
        assert_eq!(got["session"]["title"], "Weather chat");
        assert_eq!(got["session"]["pinned"], true);
        assert_eq!(
            got["session"]["last_at"], 100,
            "update must not move last_at"
        );
        update(&mut c, &s, None, Some(false)).unwrap();
        let got = read(&c, &s, None).unwrap();
        assert_eq!(
            (
                got["session"]["title"].as_str(),
                got["session"]["pinned"].as_bool()
            ),
            (Some("Weather chat"), Some(false))
        );
        assert!(update(&mut c, &s, None, None).is_err());
        assert_eq!(
            update(&mut c, "nope", Some("t".into()), None).unwrap_err(),
            "unknown session nope"
        );
    }

    #[test]
    fn delete_hides_and_tombstones() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = mk(&mut c, &cfg, "lap", "chat", "x", 100);
        let keep = mk(&mut c, &cfg, "phone", "chat", "x", 100);
        let r = delete(&mut c, &s).unwrap();
        let rev = r["rev"].as_i64().unwrap();
        let state: String = c
            .query_row("SELECT state FROM sessions WHERE id=?", [&s], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "deleting");
        let turns: i64 = c
            .query_row("SELECT count(*) FROM turns WHERE session_id=?", [&s], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(turns, 0);
        let trev: i64 = c
            .query_row("SELECT rev FROM tombstones WHERE id=?", [&s], |r| r.get(0))
            .unwrap();
        assert_eq!(trev, rev);
        // other sessions untouched
        assert_eq!(
            read(&c, &keep, None).unwrap()["turns"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(delete(&mut c, "nope").unwrap_err(), "unknown session nope");
    }

    #[test]
    fn delete_is_idempotent() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = mk(&mut c, &cfg, "lap", "chat", "x", 100);
        let a = delete(&mut c, &s).unwrap();
        let b = delete(&mut c, &s).unwrap();
        assert_eq!(a, b);
        let n: i64 = c
            .query_row("SELECT count(*) FROM tombstones", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let rev: i64 = c
            .query_row("SELECT value FROM meta WHERE key='rev'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            Some(rev),
            a["rev"].as_i64(),
            "second delete must not take a new rev"
        );
    }

    #[test]
    fn deleted_session_invisible_everywhere() {
        use crate::search::{search, SearchParams};
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "voice", None), 100).unwrap());
        append(&mut c, &cfg, ap(&s, "secret pineapple", "noted"), 101).unwrap();
        let del = delete(&mut c, &s).unwrap();
        assert!(list(&c, &ListParams::default()).unwrap()["sessions"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            read(&c, &s, None).unwrap_err(),
            format!("unknown session {s}")
        );
        let sp = SearchParams {
            query: "pineapple".into(),
            ..Default::default()
        };
        assert!(search(&c, &sp).unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            update(&mut c, &s, Some("x".into()), None).unwrap_err(),
            format!("unknown session {s}")
        );
        assert_eq!(
            append(&mut c, &cfg, ap(&s, "more", "x"), 102).unwrap_err(),
            format!("unknown session {s}")
        );
        // resolve neither returns nor extends it, even with the explicit id, inside the pause
        let r = resolve(&mut c, &cfg, &rp("lap", "voice", Some(&s)), 103).unwrap();
        assert_eq!(r["created"], true);
        assert_ne!(sid(&r), s);
        let r2 = resolve(&mut c, &cfg, &rp("lap", "voice", None), 104).unwrap();
        assert_eq!(sid(&r2), sid(&r));
        // sync: only the tombstone for the deleted one (plus the new empty session)
        let ch = crate::sync::sync(&c, 0, 500, 500).unwrap();
        let ch = ch["changes"].as_array().unwrap();
        assert!(ch.iter().all(|e| e["op"] != "turn"));
        assert!(ch.iter().all(|e| e["session"]["id"] != s.as_str()));
        assert!(ch
            .iter()
            .any(|e| e["op"] == "delete" && e["id"] == s.as_str() && e["rev"] == del["rev"]));
    }
}
