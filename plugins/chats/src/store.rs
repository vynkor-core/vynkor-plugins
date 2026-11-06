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
    tx.query_row("UPDATE meta SET value = value + 1 WHERE key = 'rev' RETURNING value", [], |r| r.get(0))
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
pub fn resolve(conn: &mut Connection, cfg: &Config, p: &ResolveParams, now: i64) -> Result<Value, String> {
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
pub fn append(conn: &mut Connection, cfg: &Config, p: AppendParams, now: i64) -> Result<Value, String> {
    if !STATUSES.contains(&p.status.as_str()) {
        return Err(format!("status must be ok, failed or cancelled, got {:?}", p.status));
    }
    let user_text = truncate_bytes(&p.user_text, cfg.text_max_bytes);
    let answer_text = truncate_bytes(&p.answer_text, cfg.text_max_bytes);
    let trace = normalize_trace(&p.tool_trace, cfg).to_string();
    let tx = conn.transaction().map_err(db_err)?;
    let count: Option<i64> = tx
        .query_row("SELECT turns FROM sessions WHERE id = ? AND state = 'live'", [&p.session_id], |r| r.get(0))
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
        let b = resolve(&mut c, &cfg, &rp("lap", "voice", None), T0 + 1000 + PAUSE_MS - 1).unwrap();
        assert_eq!(b["created"], false);
        assert_eq!(sid(&a), sid(&b));
        let c2 = resolve(&mut c, &cfg, &rp("lap", "voice", Some(&sid(&a))), T0 + 1000 + PAUSE_MS - 1).unwrap();
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
        let d = resolve(&mut c, &cfg, &rp("lap", "voice", Some(&sid(&a))), T0 + PAUSE_MS).unwrap();
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
        let b = resolve(&mut c, &cfg, &rp("lap", "chat", Some(&sid(&a))), T0 + 100 * PAUSE_MS).unwrap();
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
            .query_row("SELECT turns, last_at, rev FROM sessions WHERE id=?", [&s], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((turns, last_at), (2, T0 + 20));
        assert_eq!(Some(rev), r2["rev"].as_i64());
        let trev: i64 = c.query_row("SELECT rev FROM turns WHERE seq=2", [], |r| r.get(0)).unwrap();
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
        let n: i64 = c.query_row("SELECT count(*) FROM turns", [], |r| r.get(0)).unwrap();
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
            .query_row("SELECT user_text, answer_text, tool_trace FROM turns", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert!(u.len() <= 100 && u.ends_with('…'), "user_text len {}", u.len());
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
        let (u, t): (String, String) =
            c.query_row("SELECT user_text, tool_trace FROM turns", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
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
                        let r = append(&mut g, &cfg, ap(&s, &format!("t{t}-{i}"), "a"), T0 + i).unwrap();
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
        assert!(revs.windows(2).all(|w| w[0] < w[1]), "revs not strictly increasing");
        let distinct: i64 = c.lock().unwrap().query_row("SELECT count(DISTINCT rev) FROM turns", [], |r| r.get(0)).unwrap();
        assert_eq!(distinct, 200);
    }
}
