//! Request parsing and validation for the `notify` plugin.
//!
//! [`NotifyParams`] is the caller-facing shape of `notify_send`; every
//! field violation gets a specific, human-readable error so callers know
//! exactly what to fix.

use serde::Deserialize;

/// Hard cap on the notification message, in bytes. Keeps argv sizes sane
/// and bounds the kernel's copy of `params_json` for this action.
pub const MAX_MESSAGE_BYTES: usize = 4096;
/// Hard cap on the notification title, in bytes.
pub const MAX_TITLE_BYTES: usize = 256;
/// Accepted `urgency` values — notify-send's three levels.
pub const URGENCIES: &[&str] = &["low", "normal", "critical"];
/// Upper bound for `timeout_ms` (10 minutes). `0` = leave the provider
/// default.
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// Cap on inbox entry ids (same policy as message/title caps — reject,
/// never truncate).
pub const MAX_ID_BYTES: usize = 128;

/// Parameters for a `notify_send` action.
#[derive(Debug, Clone, Deserialize)]
pub struct NotifyParams {
    /// Delivery provider id. Defaults to `notify-send`.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Notification title; empty = no title. Capped at [`MAX_TITLE_BYTES`].
    #[serde(default)]
    pub title: String,
    /// Notification body. Required, non-empty, capped at
    /// [`MAX_MESSAGE_BYTES`].
    pub message: String,
    /// notify-send urgency (`low` | `normal` | `critical`). Optional.
    pub urgency: Option<String>,
    /// notify-send display timeout in milliseconds (`1..=MAX_TIMEOUT_MS`);
    /// `0` leaves the provider default. Optional.
    pub timeout_ms: Option<u64>,
    /// notify-send app name. Optional; falls back to
    /// `NOTIFY_PLUGIN_APP_NAME`, then `vynkor`.
    pub app_name: Option<String>,
    /// Store only — no delivery; the notification lands in the inbox and is
    /// visible later via `notify_list`. `provider`/`speak` are ignored when
    /// set.
    #[serde(default)]
    pub silent: bool,
    /// Also synthesize the notification through the `tts` plugin and play
    /// the audio on the host (best-effort).
    #[serde(default)]
    pub speak: bool,
}

fn default_provider() -> String {
    "notify-send".to_string()
}

/// Parameters for a `notify_list` action.
#[derive(Debug, Clone, Deserialize)]
pub struct ListParams {
    /// Include already-read notifications in the listing. Default false.
    #[serde(default)]
    pub include_read: bool,
}

impl ListParams {
    pub fn parse(params_json: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(params_json).map_err(|e| format!("invalid request JSON: {e}"))
    }
}

/// Parameters for `notify_mark_read` / `notify_delete`.
#[derive(Debug, Clone, Deserialize)]
pub struct IdParams {
    /// Inbox entry id from `notify_list` / a `notify_send` response.
    pub id: String,
}

impl IdParams {
    pub fn parse(params_json: &[u8]) -> Result<Self, String> {
        let params: IdParams = serde_json::from_slice(params_json)
            .map_err(|e| format!("invalid request JSON: {e}"))?;
        params.validate()?;
        Ok(params)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("id is required and must be non-empty".to_string());
        }
        if self.id.len() > MAX_ID_BYTES {
            return Err(format!(
                "id is {} bytes, exceeding the {MAX_ID_BYTES}-byte cap",
                self.id.len()
            ));
        }
        Ok(())
    }
}

impl NotifyParams {
    /// Parse and validate the action's `params_json`. Every violation gets a
    /// specific, human-readable error.
    pub fn parse(params_json: &[u8]) -> Result<Self, String> {
        let params: NotifyParams = serde_json::from_slice(params_json)
            .map_err(|e| format!("invalid request JSON: {e}"))?;
        params.validate()?;
        Ok(params)
    }

    /// Field-level validation: lengths, enums, ranges.
    pub fn validate(&self) -> Result<(), String> {
        if self.message.is_empty() {
            return Err("message is required and must be non-empty".to_string());
        }
        if self.message.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "message is {} bytes, exceeding the {MAX_MESSAGE_BYTES}-byte cap",
                self.message.len()
            ));
        }
        if self.title.len() > MAX_TITLE_BYTES {
            return Err(format!(
                "title is {} bytes, exceeding the {MAX_TITLE_BYTES}-byte cap",
                self.title.len()
            ));
        }
        if let Some(urgency) = &self.urgency {
            if !URGENCIES.contains(&urgency.as_str()) {
                return Err(format!(
                    "invalid urgency '{urgency}': must be one of {}",
                    URGENCIES.join(", ")
                ));
            }
        }
        if let Some(timeout) = self.timeout_ms {
            if timeout != 0 && !(1..=MAX_TIMEOUT_MS).contains(&timeout) {
                return Err(format!(
                    "invalid timeout_ms {timeout}: must be in 1..={MAX_TIMEOUT_MS} (0 = leave the provider default)"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(json: &str) -> Result<NotifyParams, String> {
        NotifyParams::parse(json.as_bytes())
    }

    #[test]
    fn accepts_valid_params_with_all_fields() {
        let p = params(
            r#"{"provider":"notify-send","title":"Build done","message":"cargo build succeeded","urgency":"normal","timeout_ms":5000,"app_name":"ci"}"#,
        )
        .unwrap();
        assert_eq!(p.provider, "notify-send");
        assert_eq!(p.title, "Build done");
        assert_eq!(p.message, "cargo build succeeded");
        assert_eq!(p.urgency.as_deref(), Some("normal"));
        assert_eq!(p.timeout_ms, Some(5000));
        assert_eq!(p.app_name.as_deref(), Some("ci"));
    }

    #[test]
    fn defaults_apply_when_optional_fields_omitted() {
        let p = params(r#"{"message":"hi"}"#).unwrap();
        assert_eq!(p.provider, "notify-send");
        assert_eq!(p.title, "");
        assert_eq!(p.urgency, None);
        assert_eq!(p.timeout_ms, None);
        assert_eq!(p.app_name, None);
    }

    #[test]
    fn rejects_missing_message() {
        let err = params(r#"{"title":"no body"}"#).unwrap_err();
        assert!(err.contains("message"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_message() {
        let err = params(r#"{"message":""}"#).unwrap_err();
        assert!(err.contains("message is required"), "error was: {err}");
    }

    #[test]
    fn rejects_oversize_message() {
        let body = format!(r#"{{"message":"{}"}}"#, "x".repeat(MAX_MESSAGE_BYTES + 1));
        let err = params(&body).unwrap_err();
        assert!(err.contains("4096"), "error was: {err}");
    }

    #[test]
    fn accepts_message_exactly_at_cap() {
        let body = format!(r#"{{"message":"{}"}}"#, "x".repeat(MAX_MESSAGE_BYTES));
        assert!(params(&body).is_ok());
    }

    #[test]
    fn rejects_oversize_title() {
        let body = format!(
            r#"{{"message":"hi","title":"{}"}}"#,
            "x".repeat(MAX_TITLE_BYTES + 1)
        );
        let err = params(&body).unwrap_err();
        assert!(err.contains("256"), "error was: {err}");
    }

    #[test]
    fn rejects_unknown_urgency() {
        let err = params(r#"{"message":"hi","urgency":"mega"}"#).unwrap_err();
        assert!(err.contains("urgency"), "error was: {err}");
        assert!(err.contains("low, normal, critical"), "error was: {err}");
    }

    #[test]
    fn accepts_all_valid_urgencies() {
        for urgency in URGENCIES {
            let body = format!(r#"{{"message":"hi","urgency":"{urgency}"}}"#);
            assert!(params(&body).is_ok(), "urgency {urgency} rejected");
        }
    }

    #[test]
    fn accepts_timeout_zero_as_provider_default() {
        assert!(params(r#"{"message":"hi","timeout_ms":0}"#).is_ok());
    }

    #[test]
    fn rejects_oversize_timeout() {
        let err = params(r#"{"message":"hi","timeout_ms":600001}"#).unwrap_err();
        assert!(err.contains("timeout_ms"), "error was: {err}");
        assert!(err.contains("600000"), "error was: {err}");
    }

    #[test]
    fn silent_and_speak_default_to_false() {
        let p = params(r#"{"message":"hi"}"#).unwrap();
        assert!(!p.silent);
        assert!(!p.speak);

        let p = params(r#"{"message":"hi","silent":true,"speak":true}"#).unwrap();
        assert!(p.silent);
        assert!(p.speak);
    }

    #[test]
    fn list_params_include_read_defaults_to_false() {
        let p = ListParams::parse(b"{}").unwrap();
        assert!(!p.include_read);

        let p = ListParams::parse(br#"{"include_read":true}"#).unwrap();
        assert!(p.include_read);
    }

    #[test]
    fn id_params_accepts_valid_id() {
        let p = IdParams::parse(br#"{"id":"1700000000000-3"}"#).unwrap();
        assert_eq!(p.id, "1700000000000-3");
    }

    #[test]
    fn id_params_rejects_missing_id() {
        let err = IdParams::parse(b"{}").unwrap_err();
        assert!(err.contains("id"), "error was: {err}");
    }

    #[test]
    fn id_params_rejects_empty_id() {
        let err = IdParams::parse(br#"{"id":""}"#).unwrap_err();
        assert!(err.contains("id is required"), "error was: {err}");
    }

    #[test]
    fn id_params_rejects_oversize_id() {
        let body = format!(r#"{{"id":"{}"}}"#, "x".repeat(MAX_ID_BYTES + 1));
        let err = IdParams::parse(body.as_bytes()).unwrap_err();
        assert!(err.contains("128"), "error was: {err}");
    }
}

// ---- push_send (INT-02: ntfy/Gotify push to the phone) ---------------------

/// Comma-separated host allowlist for `push_send`'s `server` param.
/// Default-deny except `ntfy.sh`; self-hosted Gotify/ntfy hosts are added
/// by the operator here. A caller-controlled URL would let any plugin with
/// the notify permission exfiltrate arbitrary text to arbitrary hosts.
pub const PUSH_SERVERS_ENV: &str = "NOTIFY_PLUGIN_PUSH_SERVERS";
pub const PUSH_DEFAULT_SERVERS: &str = "ntfy.sh";
/// Auth token for ntfy (sent as `Authorization: Bearer …` when set).
pub const NTFY_TOKEN_ENV: &str = "NOTIFY_PLUGIN_NTFY_TOKEN";
/// App token for Gotify (sent as `X-Gotify-Token` when set).
pub const GOTIFY_TOKEN_ENV: &str = "NOTIFY_PLUGIN_GOTIFY_TOKEN";
/// Cap on `tags` entries (ntfy tags header / ignored by gotify).
pub const MAX_PUSH_TAGS: usize = 8;
/// Per-request HTTP timeout through `network` (clamped).
pub const MAX_PUSH_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushProvider {
    Ntfy,
    Gotify,
}

impl PushProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            PushProvider::Ntfy => "ntfy",
            PushProvider::Gotify => "gotify",
        }
    }
}

/// Parameters for a `push_send` action.
#[derive(Debug, Clone, Deserialize)]
pub struct PushParams {
    /// `ntfy` (default) or `gotify`.
    #[serde(default)]
    pub provider: Option<String>,
    /// Push server host (`host` or `host:port`, no scheme/path). Must be on
    /// the [`PUSH_SERVERS_ENV`] allowlist; defaults to the first entry.
    #[serde(default)]
    pub server: Option<String>,
    /// ntfy topic (required for ntfy, rejected for gotify). `[A-Za-z0-9_-]{1,64}`.
    #[serde(default)]
    pub topic: Option<String>,
    /// Optional; omitted from the push payload when empty.
    #[serde(default)]
    pub title: String,
    /// Required, non-empty, capped at [`MAX_MESSAGE_BYTES`].
    pub message: String,
    /// `1..=5` — ntfy priority levels / gotify priority int. Optional.
    pub priority: Option<u8>,
    /// Up to [`MAX_PUSH_TAGS`] short strings (ntfy only).
    #[serde(default)]
    pub tags: Vec<String>,
    /// HTTP timeout in ms through `network` (`1..=MAX_PUSH_TIMEOUT_MS`,
    /// default 10_000).
    pub timeout_ms: Option<u64>,
}

/// Parse the operator's host allowlist: trimmed, lowercased, non-empty.
pub fn parse_push_servers(raw: &str) -> Vec<String> {
    let source = if raw.trim().is_empty() {
        PUSH_DEFAULT_SERVERS
    } else {
        raw
    };
    let mut out = Vec::new();
    for part in source.split(',') {
        let host = part.trim().to_ascii_lowercase();
        if !host.is_empty() && !out.contains(&host) {
            out.push(host);
        }
    }
    out
}

/// Exact host(:port) match against the allowlist, case-insensitive.
pub fn is_allowed_push_server(server: &str, allowed: &[String]) -> bool {
    let server = server.trim().to_ascii_lowercase();
    allowed.iter().any(|h| h == &server)
}

fn validate_topic(topic: &str) -> Result<(), String> {
    let ok_len = !topic.is_empty() && topic.len() <= 64;
    let ok_chars = topic
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok_len && ok_chars {
        Ok(())
    } else {
        Err("params.topic must be 1..=64 chars of [A-Za-z0-9_-]".to_string())
    }
}

/// Parse and validate `params_json` for `push_send`.
pub fn parse_push_params(
    params_json: &[u8],
    allowed_servers: &[String],
) -> Result<PushParams, String> {
    let p: PushParams = serde_json::from_slice(params_json).map_err(|e| {
        format!(
            "invalid params for push_send, expected {{provider?, server?, topic?, \
             title?, message, priority?, tags?, timeout_ms?}}: {e}"
        )
    })?;
    let provider = match p.provider.as_deref() {
        None | Some("") | Some("ntfy") => PushProvider::Ntfy,
        Some("gotify") => PushProvider::Gotify,
        Some(other) => return Err(format!("unknown push provider: {other}")),
    };
    if p.title.len() > MAX_TITLE_BYTES {
        return Err(format!("params.title exceeds {MAX_TITLE_BYTES} bytes"));
    }
    if p.message.trim().is_empty() {
        return Err("push_send requires a non-empty message".to_string());
    }
    if p.message.len() > MAX_MESSAGE_BYTES {
        return Err(format!("params.message exceeds {MAX_MESSAGE_BYTES} bytes"));
    }
    let server = p
        .server
        .clone()
        .unwrap_or_else(|| allowed_servers[0].clone());
    if !is_allowed_push_server(&server, allowed_servers) {
        return Err(format!(
            "params.server '{server}' is not in the operator's {} allowlist",
            PUSH_SERVERS_ENV
        ));
    }
    match provider {
        PushProvider::Ntfy => {
            let topic = p.topic.as_deref().unwrap_or_default();
            validate_topic(topic)?;
        }
        PushProvider::Gotify => {
            if p.topic.is_some() {
                return Err("params.topic is ntfy-only; gotify uses its app token".to_string());
            }
        }
    }
    if let Some(priority) = p.priority {
        if !(1..=5).contains(&priority) {
            return Err("params.priority must be between 1 and 5".to_string());
        }
    }
    if p.tags.len() > MAX_PUSH_TAGS {
        return Err(format!("params.tags exceeds {MAX_PUSH_TAGS} entries"));
    }
    for tag in &p.tags {
        if tag.is_empty() || tag.len() > 64 {
            return Err("params.tags entries must be 1..=64 bytes".to_string());
        }
    }
    if let Some(t) = p.timeout_ms {
        if t == 0 || t > MAX_PUSH_TIMEOUT_MS {
            return Err(format!(
                "params.timeout_ms must be between 1 and {MAX_PUSH_TIMEOUT_MS}"
            ));
        }
    }
    Ok(p)
}
