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
/// - `kind` defaults to `voice`; device is lower-cased, an empty source
///   becomes `"local"`, source is capped at [`SOURCE_MAX_CHARS`].
pub fn origin(
    caller: &str,
    kind: Option<&str>,
    source: Option<&str>,
    local_device: &str,
) -> Origin {
    let (device, default_source) = match caller.split_once('.') {
        Some((dev, plugin)) if !dev.is_empty() => (dev, plugin),
        Some((_, plugin)) => (local_device, plugin),
        None => (local_device, caller),
    };
    let source = source.filter(|s| !s.is_empty()).unwrap_or(default_source);
    let source = if source.is_empty() { "local" } else { source };
    Origin {
        device: device.to_lowercase(),
        kind: kind.unwrap_or("voice").to_string(),
        source: source.chars().take(SOURCE_MAX_CHARS).collect(),
    }
}

/// Cap for the derived/explicit `source` label (chars).
pub const SOURCE_MAX_CHARS: usize = 64;

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

/// First-line cap for errors of fs/exec/shell tools (chars).
const OMIT_ERROR_MAX_CHARS: usize = 60;
const REDACTED: &str = "[redacted]";

/// Tool names whose arguments and results are secret by nature.
const SENSITIVE_TOOL_WORDS: [&str; 4] = ["secret", "password", "token", "credential"];
/// Substrings (lowercase) of JSON keys whose values are masked everywhere.
const SENSITIVE_KEY_WORDS: [&str; 11] = [
    "value", "password", "passwd", "token", "secret", "api_key", "apikey", "authorization", "auth",
    "cookie", "private_key",
];

/// Tool names may be target-prefixed (`dev-phone.fs_read`, `laptop.exec`):
/// classify on the base name after the last `.`, lower-cased.
fn base_name(tool: &str) -> String {
    tool.rsplit('.').next().unwrap_or(tool).to_lowercase()
}

fn is_sensitive_tool(tool: &str) -> bool {
    let t = base_name(tool);
    SENSITIVE_TOOL_WORDS.iter().any(|w| t.contains(w))
}

fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_lowercase();
    SENSITIVE_KEY_WORDS.iter().any(|w| k.contains(w))
}

/// Recursively replace the value of every sensitive key with `[redacted]`.
fn mask_keys(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_sensitive_key(k) {
                    *val = Value::String(REDACTED.to_string());
                } else {
                    mask_keys(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_keys),
        _ => {}
    }
}

/// Mask sensitive keys when `text` is JSON; other text is returned as is.
fn mask_json_text(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(mut v) => {
            mask_keys(&mut v);
            v.to_string()
        }
        Err(_) => text.to_string(),
    }
}

/// Strip secrets from a tool call before it enters the trace (the trace is
/// synced to every client device). Returns `(args, result)` as text, NOT yet
/// truncated: redaction must run on the whole value, then the cut.
///
/// - secret-like tool (`secret_*`, contains secret/password/token/credential):
///   args keep only their key names, every value `[redacted]`; result `[redacted]`;
/// - every tool: values of sensitive keys are masked recursively in args and,
///   when it parses as JSON, in the result.
pub fn redact(tool: &str, args: &Value, result: &str) -> (String, String) {
    (redact_args(tool, args), redact_result(tool, result))
}

/// Tools whose output is file or shell content: never copied into the trace.
fn is_omit_tool(tool: &str) -> bool {
    let t = base_name(tool);
    t.starts_with("fs_") || t.starts_with("exec") || t.starts_with("shell")
}

fn redact_args(tool: &str, args: &Value) -> String {
    if is_sensitive_tool(tool) {
        let keys_only = match args {
            Value::Object(map) => Value::Object(
                map.keys().map(|k| (k.clone(), Value::String(REDACTED.to_string()))).collect(),
            ),
            Value::Null => Value::Null,
            _ => Value::String(REDACTED.to_string()),
        };
        return keys_only.to_string();
    }
    let mut masked = args.clone();
    mask_keys(&mut masked);
    scrub_text(&masked.to_string())
}

fn redact_result(tool: &str, result: &str) -> String {
    if is_sensitive_tool(tool) {
        REDACTED.to_string()
    } else if is_omit_tool(tool) {
        "[omitted]".to_string()
    } else {
        scrub_text(&mask_json_text(result))
    }
}

/// Error text: same masking as results. For fs/exec/shell tools the error
/// body may quote file or command output, so only the first line survives:
/// scrubbed, capped at 60 chars, and `[omitted]` if the scrub changed anything.
fn redact_error(tool: &str, err: &str) -> String {
    if is_sensitive_tool(tool) {
        REDACTED.to_string()
    } else if is_omit_tool(tool) {
        let first = err.lines().next().unwrap_or("");
        if scrub_text(first) != first {
            "[omitted]".to_string()
        } else {
            first.chars().take(OMIT_ERROR_MAX_CHARS).collect()
        }
    } else {
        scrub_text(&mask_json_text(err))
    }
}

fn scrub_regexes() -> &'static [regex::Regex; 3] {
    static RES: std::sync::OnceLock<[regex::Regex; 3]> = std::sync::OnceLock::new();
    RES.get_or_init(|| {
        [
            regex::Regex::new(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]+").unwrap(),
            regex::Regex::new(
                r"(?i)[A-Za-z0-9_.-]*(?:key|token|secret|passw\w*|authorization)[A-Za-z0-9_.-]*\s*[:=]\s*\S+",
            )
            .unwrap(),
            regex::Regex::new(r"[A-Za-z0-9+_=-]{32,}").unwrap(),
        ]
    })
}

/// Pattern pass for text that is not (or not only) JSON: `Bearer <tok>`,
/// `name: value` / `NAME=value` pairs whose name looks secret (`.env` files,
/// headers), and long token-like runs (>= 32 chars with a letter and a digit;
/// `/` is not part of the class, so file paths are not eaten).
fn scrub_text(text: &str) -> String {
    let [bearer, pair, long] = scrub_regexes();
    let t = bearer.replace_all(text, REDACTED);
    let t = pair.replace_all(&t, REDACTED);
    long.replace_all(&t, |c: &regex::Captures| {
        let m = &c[0];
        if m.bytes().any(|b| b.is_ascii_digit()) && m.bytes().any(|b| b.is_ascii_alphabetic()) {
            REDACTED.to_string()
        } else {
            m.to_string()
        }
    })
    .into_owned()
}

impl TraceRec {
    /// A call that ran (or failed) after `ms` milliseconds. Secrets are
    /// redacted first, then every text is cut to [`TRACE_TEXT_MAX_CHARS`].
    pub fn new(tool: &str, args: &Value, outcome: Result<&str, &str>, ms: u64) -> Self {
        let args_text = redact_args(tool, args);
        let (result, error) = match outcome {
            Ok(body) => (trace_text(&redact_result(tool, body)), None),
            Err(e) => (String::new(), Some(trace_text(&redact_error(tool, e)))),
        };
        Self { tool: tool.to_string(), args: trace_text(&args_text), result, error, ms }
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
            if q.trim().is_empty() || a.trim().is_empty() {
                return None;
            }
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
    fn redact_masks_sensitive_tools_and_keys() {
        let args = json!({"name": "db_pass", "opts": {"Password": "p1", "keep": "ok"}});
        let (a, r) = redact("secret_get", &args, "hunter2");
        assert_eq!(a, r#"{"name":"[redacted]","opts":"[redacted]"}"#);
        assert_eq!(r, "[redacted]");
        for tool in ["vault_TOKEN_read", "get_Credentials", "set_password"] {
            assert_eq!(redact(tool, &args, "x").1, "[redacted]", "{tool}");
        }
        let (a, r) = redact(
            "http_get",
            &json!({"url": "u", "headers": [{"Authorization": "Bearer abc", "x": "y"}]}),
            r#"{"data": {"api_key": "k", "n": 1}, "cookie": "c"}"#,
        );
        assert!(!a.contains("Bearer abc") && a.contains("\"x\":\"y\""), "{a}");
        assert!(!r.contains("\"k\""), "{r}");
        assert!(r.contains(r#""n":1"#) && !r.contains(r#""c""#), "{r}");
        // non-JSON result of a normal tool is untouched
        assert_eq!(redact("http_get", &json!({}), "plain text").1, "plain text");
    }

    #[test]
    fn redaction_runs_before_truncation() {
        // The secret sits past the 200-char cut in a JSON value under a
        // sensitive key: masking first must still remove it entirely.
        let long = format!(r#"{{"pad": "{}", "token": "SECRET-TAIL"}}"#, "p".repeat(300));
        let rec = TraceRec::new("http_get", &json!({}), Ok(&long), 1);
        assert!(!rec.result.contains("SECRET-TAIL"));
        let rec = TraceRec::new("http_get", &json!({"token": "T0P"}), Err(r#"{"auth":"A1"}"#), 1);
        assert!(!rec.args.contains("T0P"));
        assert!(!rec.error.unwrap().contains("A1"));
    }

    #[test]
    fn origin_normalizes_device_and_source() {
        assert_eq!(origin("laptop.", None, None, "host"), o("laptop", "voice", "local"));
        assert_eq!(origin("Laptop.desktop", None, None, "host"), o("laptop", "voice", "desktop"));
        assert_eq!(origin("", None, None, "HOST"), o("host", "voice", "local"));
        assert_eq!(origin(".desktop", None, None, "host"), o("host", "voice", "desktop"));
        let long = format!("dev.{}", "s".repeat(100));
        assert_eq!(origin(&long, None, None, "host").source.chars().count(), SOURCE_MAX_CHARS);
        let src = origin("dev.x", None, Some(&"é".repeat(100)), "host").source;
        assert_eq!(src.chars().count(), SOURCE_MAX_CHARS);
    }

    #[test]
    fn plain_text_results_are_scrubbed() {
        let env = "DB_HOST=localhost\nDB_PASSWORD=hunter2\nAPI_KEY: sk-abc123\nok=1\n";
        let r = redact("http_get", &json!({}), env).1;
        assert!(!r.contains("hunter2") && !r.contains("sk-abc123"), "{r}");
        assert!(r.contains("DB_HOST=localhost") && r.contains("ok=1"), "{r}");
        let r = redact("http_get", &json!({}), "Authorization: Bearer abc.def-123").1;
        assert!(!r.contains("abc.def"), "{r}");
        let r = redact("http_get", &json!({}), "Bearer abc.def-123 then more").1;
        assert!(!r.contains("abc.def") && r.contains("then more"), "{r}");
        let hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let r = redact("http_get", &json!({}), &format!("hash {hex} end")).1;
        assert!(!r.contains(hex) && r.contains("end"), "{r}");
        let b64 = "dGhpcyBpcyBhIHNlY3JldCB0b2tlbiB2YWx1ZQ0123456789";
        assert!(!redact("http_get", &json!({}), b64).1.contains(b64));
        // paths and plain prose survive
        let path = "/home/behzod/projects/vynkor-core/vynkor-plugins-wt-a1/plugins/agent";
        assert_eq!(redact("http_get", &json!({}), path).1, path);
        // errors get the same pass
        let rec = TraceRec::new("http_get", &json!({}), Err("denied: token=abc123"), 1);
        assert!(!rec.error.unwrap().contains("abc123"));
    }

    #[test]
    fn fs_and_exec_results_are_omitted_but_errors_kept() {
        for tool in ["fs_read", "fs_list", "exec_run", "shell_exec", "Shell"] {
            assert_eq!(redact(tool, &json!({"path": "/x"}), "SECRET FILE BODY").1, "[omitted]", "{tool}");
        }
        let rec = TraceRec::new("fs_read", &json!({"path": "/etc/x"}), Ok("body"), 1);
        assert_eq!(rec.result, "[omitted]");
        assert!(rec.args.contains("/etc/x"), "args stay: {}", rec.args);
        let rec = TraceRec::new("fs_read", &json!({}), Err("no such file"), 1);
        assert_eq!(rec.error.as_deref(), Some("no such file"));
    }

    #[test]
    fn target_prefixed_tool_names_are_classified_by_base_name() {
        assert_eq!(redact("dev-phone.fs_read", &json!({}), "BODY").1, "[omitted]");
        assert_eq!(redact("laptop.exec", &json!({}), "BODY").1, "[omitted]");
        let (a, r) = redact("dev-x.secret_get", &json!({"name": "db_pass"}), "hunter2");
        assert_eq!(r, "[redacted]");
        assert_eq!(a, r#"{"name":"[redacted]"}"#);
        assert_eq!(redact("Dev-X.Secret_Get", &json!({}), "hunter2").1, "[redacted]");
        // prefix text is not the tool: a plain tool on a device called "fs_box" is not omitted
        assert_eq!(redact("fs_box.ping", &json!({}), "pong").1, "pong");
    }

    #[test]
    fn omit_tool_errors_keep_only_a_scrubbed_first_line() {
        let env_err = "command failed:\nDB_PASSWORD=hunter2\nAPI_KEY=abc";
        let rec = TraceRec::new("exec_run", &json!({}), Err(env_err), 1);
        assert_eq!(rec.error.as_deref(), Some("command failed:"), "only the first line");
        let rec = TraceRec::new("exec_run", &json!({}), Err("DB_PASSWORD=hunter2\nrest"), 1);
        assert_eq!(rec.error.as_deref(), Some("[omitted]"), "scrub changed the line");
        let long = format!("{} tail", "e".repeat(100));
        let rec = TraceRec::new("laptop.shell_exec", &json!({}), Err(&long), 1);
        assert_eq!(rec.error.unwrap().chars().count(), OMIT_ERROR_MAX_CHARS);
        // ordinary tools keep their (scrubbed) multi-line error
        let rec = TraceRec::new("http_get", &json!({}), Err("a\nb"), 1);
        assert_eq!(rec.error.as_deref(), Some("a\nb"));
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
