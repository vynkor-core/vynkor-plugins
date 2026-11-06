//! Chat-session glue between `agent` and the `chats` plugin: who is asking
//! (device / kind / source), which session the goal belongs to, prior turns
//! for the LLM context, and the finished turn written back.
//!
//! A turn never fails because of `chats`: every error here degrades to
//! "no session" and the goal runs exactly as it did before sessions existed.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::Rpc;

/// Per-text cap for one history message (chars).
pub const HISTORY_TEXT_MAX_CHARS: usize = 2000;
/// Cap on all history text together (bytes); oldest turns go first.
pub const HISTORY_TOTAL_MAX_BYTES: usize = 8000;

/// Where a goal came from. `device` and `source` are derived from the
/// kernel-stamped `caller_plugin_id`, which a plugin cannot forge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub device: String,
    pub kind: String,
    pub source: String,
}

/// Derive the origin of a goal.
///
/// - `caller` = `<device>.<plugin>` (relayed from a device): device is the
///   part before the first dot, source defaults to the plugin part;
/// - `caller` without a dot (local plugin) or empty: device is
///   `local_device`, source defaults to the caller (`"local"` when empty);
/// - `kind` defaults to `voice`.
pub fn origin(
    caller: &str,
    kind: Option<&str>,
    source: Option<&str>,
    local_device: &str,
) -> Origin {
    let (device, default_source) = match caller.split_once('.') {
        Some((dev, plugin)) if !dev.is_empty() => (dev.to_string(), plugin.to_string()),
        _ if caller.is_empty() => (local_device.to_string(), "local".to_string()),
        _ => (local_device.to_string(), caller.to_string()),
    };
    Origin {
        device,
        kind: kind.unwrap_or("voice").to_string(),
        source: source.map(str::to_string).unwrap_or(default_source),
    }
}

/// Cap for `args` / `result` / `error` kept in the trace (chars).
pub const TRACE_TEXT_MAX_CHARS: usize = 200;

/// One tool call of a goal, as stored in `GoalDoc.tool_trace` and sent to
/// `chats` as `tool_trace[]`. Text fields are already cut to
/// [`TRACE_TEXT_MAX_CHARS`] (+ `…`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceRec {
    pub tool: String,
    pub args: String,
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub ms: u64,
}

/// Cut to [`TRACE_TEXT_MAX_CHARS`] characters (not bytes), marking the cut with `…`.
pub fn trace_text(s: &str) -> String {
    if s.chars().count() <= TRACE_TEXT_MAX_CHARS {
        s.to_string()
    } else {
        let cut: String = s.chars().take(TRACE_TEXT_MAX_CHARS).collect();
        format!("{cut}…")
    }
}

impl TraceRec {
    /// A call that ran (or failed) after `ms` milliseconds.
    pub fn new(tool: &str, args: &Value, outcome: Result<&str, &str>, ms: u64) -> Self {
        let (result, error) = match outcome {
            Ok(body) => (trace_text(body), None),
            Err(e) => (String::new(), Some(trace_text(e))),
        };
        Self { tool: tool.to_string(), args: trace_text(&args.to_string()), result, error, ms }
    }
}

/// `chats` turn status for a goal status; `None` = not finished, write nothing.
pub fn turn_status(goal_status: &str) -> Option<&'static str> {
    match goal_status {
        "completed" => Some("ok"),
        "declined" => Some("cancelled"),
        "error" | "max_steps_reached" => Some("failed"),
        _ => None,
    }
}

/// Write the finished goal as a turn of its session. No-op without a
/// session or while the goal is still running / awaiting confirmation.
/// Failures are logged, never returned: the turn already happened.
pub async fn append_turn(rpc: &Rpc, timeout_ms: u32, doc: &crate::store::GoalDoc) {
    let (Some(session_id), Some(status)) = (doc.session_id.as_deref(), turn_status(&doc.status))
    else {
        return;
    };
    let answer = if !doc.final_answer.is_empty() {
        doc.final_answer.clone()
    } else if !doc.error.is_empty() {
        doc.error.clone()
    } else {
        format!("Stopped without an answer ({}).", doc.status)
    };
    let params = json!({
        "session_id": session_id,
        "user_text": doc.goal,
        "answer_text": answer,
        "status": status,
        "tool_trace": doc.tool_trace,
        "goal_id": doc.id,
    });
    if let Err(e) = rpc.call("chat_turn_append", params, timeout_ms).await {
        warn_chats("turn append", &e, &MISSING_WARNED);
    }
}

/// A resolved chat session.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub id: String,
    /// True when `chats` just created the session (nothing to read yet).
    pub created: bool,
}

/// `chats` is not installed: the kernel answers `action not found`.
pub fn is_missing_plugin(err: &str) -> bool {
    err.contains("action not found")
}

static MISSING_WARNED: AtomicBool = AtomicBool::new(false);

/// Log a failed `chats` call. A missing plugin is an expected deployment
/// state, so it is logged once per process (`flag`); anything else every time.
/// Returns whether a line was printed.
fn warn_chats(what: &str, err: &str, flag: &AtomicBool) -> bool {
    if is_missing_plugin(err) {
        if flag.swap(true, Ordering::Relaxed) {
            return false;
        }
        eprintln!("[agent] chats plugin unavailable ({err}); goals run without sessions");
        return true;
    }
    eprintln!("[agent] chats {what} failed, continuing without session: {err}");
    true
}

/// Find or create the session for this goal. `None` on any failure
/// (missing plugin, error, timeout): the goal then runs without a session.
pub async fn resolve(
    rpc: &Rpc,
    timeout_ms: u32,
    o: &Origin,
    session_id: Option<&str>,
) -> Option<Resolved> {
    let mut params = json!({"device": o.device, "kind": o.kind, "source": o.source});
    if let Some(sid) = session_id {
        params["session_id"] = json!(sid);
    }
    match rpc.call("chat_session_resolve", params, timeout_ms).await {
        Ok(v) => {
            let id = v.get("session_id").and_then(Value::as_str)?.to_string();
            let created = v.get("created").and_then(Value::as_bool).unwrap_or(false);
            Some(Resolved { id, created })
        }
        Err(e) => {
            warn_chats("session resolve", &e, &MISSING_WARNED);
            None
        }
    }
}

fn clip(s: &str) -> String {
    if s.chars().count() <= HISTORY_TEXT_MAX_CHARS {
        s.to_string()
    } else {
        let cut: String = s.chars().take(HISTORY_TEXT_MAX_CHARS).collect();
        format!("{cut}…")
    }
}

/// Clip each text and drop the oldest pairs until the total fits the budget.
pub fn fit_history(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> =
        pairs.into_iter().map(|(q, a)| (clip(&q), clip(&a))).collect();
    let size = |p: &[(String, String)]| p.iter().map(|(q, a)| q.len() + a.len()).sum::<usize>();
    while !pairs.is_empty() && size(&pairs) > HISTORY_TOTAL_MAX_BYTES {
        pairs.remove(0);
    }
    pairs
}

/// Last `last` turns of a session as (question, answer), oldest first.
/// Only successful turns (`status == "ok"`) are kept. Empty on any failure.
pub async fn history(
    rpc: &Rpc,
    timeout_ms: u32,
    session_id: &str,
    last: u32,
) -> Vec<(String, String)> {
    let v = match rpc
        .call("chat_session_read", json!({"id": session_id, "last": last}), timeout_ms)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn_chats("session read", &e, &MISSING_WARNED);
            return Vec::new();
        }
    };
    let mut turns: Vec<&Value> = v
        .get("turns")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    turns.sort_by_key(|t| t.get("seq").and_then(Value::as_i64).unwrap_or(0));
    let pairs = turns
        .into_iter()
        .filter(|t| t.get("status").and_then(Value::as_str) == Some("ok"))
        .filter_map(|t| {
            let q = t.get("user_text").and_then(Value::as_str)?;
            let a = t.get("answer_text").and_then(Value::as_str)?;
            Some((q.to_string(), a.to_string()))
        })
        .collect();
    fit_history(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(device: &str, kind: &str, source: &str) -> Origin {
        Origin { device: device.into(), kind: kind.into(), source: source.into() }
    }

    #[test]
    fn turn_status_maps_every_goal_status() {
        assert_eq!(turn_status("completed"), Some("ok"));
        assert_eq!(turn_status("declined"), Some("cancelled"));
        assert_eq!(turn_status("error"), Some("failed"));
        assert_eq!(turn_status("max_steps_reached"), Some("failed"));
        assert_eq!(turn_status("running"), None);
        assert_eq!(turn_status("needs_confirmation"), None);
    }

    #[test]
    fn trace_text_cuts_by_chars_not_bytes() {
        assert_eq!(trace_text("short"), "short");
        let cyr = "ж".repeat(TRACE_TEXT_MAX_CHARS);
        assert_eq!(trace_text(&cyr), cyr, "exactly at the cap is kept");
        let cut = trace_text(&"ж".repeat(TRACE_TEXT_MAX_CHARS + 1));
        assert_eq!(cut.chars().count(), TRACE_TEXT_MAX_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn missing_plugin_is_detected_and_warned_once() {
        assert!(is_missing_plugin("chat_session_resolve failed: action not found"));
        assert!(!is_missing_plugin("chat_session_resolve timed out after 2000 ms"));
        let flag = AtomicBool::new(false);
        let missing = "chat_session_resolve failed: action not found";
        assert!(warn_chats("resolve", missing, &flag), "first warns");
        assert!(!warn_chats("read", missing, &flag), "second is silent");
        let other = "chat_session_resolve timed out after 2000 ms";
        assert!(warn_chats("resolve", other, &flag), "other errors always warn");
        assert!(warn_chats("resolve", other, &flag));
    }

    #[test]
    fn fit_history_clips_texts_and_drops_oldest_first() {
        let long = "x".repeat(HISTORY_TEXT_MAX_CHARS + 50);
        let fitted = fit_history(vec![(long.clone(), "a".into())]);
        assert_eq!(fitted[0].0.chars().count(), HISTORY_TEXT_MAX_CHARS + 1);
        assert!(fitted[0].0.ends_with('…'));
        // 5 pairs of ~2000 bytes each exceed 8000: the oldest go first.
        let pairs: Vec<(String, String)> =
            (0..5).map(|i| (format!("{i}{}", "q".repeat(999)), "a".repeat(1000))).collect();
        let fitted = fit_history(pairs);
        assert_eq!(fitted.len(), 4);
        assert!(fitted[0].0.starts_with('1'), "oldest dropped, newest kept");
        assert!(fitted[3].0.starts_with('4'));
    }

    #[test]
    fn origin_from_relayed_device_caller() {
        assert_eq!(origin("laptop.desktop", None, None, "host"), o("laptop", "voice", "desktop"));
    }

    #[test]
    fn origin_from_local_plugin_caller() {
        assert_eq!(origin("telegram", Some("chat"), None, "host"), o("host", "chat", "telegram"));
    }

    #[test]
    fn origin_from_empty_caller_uses_explicit_source() {
        assert_eq!(origin("", None, Some("ctl"), "mbp"), o("mbp", "voice", "ctl"));
        assert_eq!(origin("", None, None, "mbp"), o("mbp", "voice", "local"));
    }
}
