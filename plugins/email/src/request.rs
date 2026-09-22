//! Parse + validate the JSON body of an `email_send` `ActionRequest`.

/// Hard ceiling on the subject length, in chars. Kept tighter than RFC 5322's
/// 998-char line limit on purpose: short subjects are a deliverability nicety
/// and bound the headers handed to the SMTP server.
pub const MAX_SUBJECT_CHARS: usize = 200;

/// Hard ceiling on the body length, in chars.
pub const MAX_BODY_CHARS: usize = 10_000;

/// Default SMTP submission port when the caller omits `smtp_port`.
pub const DEFAULT_SMTP_PORT: u16 = 587;

/// Default SMTP host when the caller omits `smtp_host`. Tests and stub mode
/// never actually connect, so this only matters for real sends.
pub const DEFAULT_SMTP_HOST: &str = "localhost";

/// Default `from` address when the caller omits both `from` and `smtp_user`.
pub const DEFAULT_FROM: &str = "vynkor@localhost";

/// Default IMAP host when the caller omits `imap_host`.
pub const DEFAULT_IMAP_HOST: &str = "localhost";

/// Default IMAP port (implicit TLS) when the caller omits `imap_port`.
pub const DEFAULT_IMAP_PORT: u16 = 993;

/// Default mailbox name when the caller omits `mailbox`.
pub const DEFAULT_MAILBOX: &str = "INBOX";

/// Default and maximum number of messages to return for `email_list`.
pub const DEFAULT_LIST_LIMIT: usize = 10;
pub const MAX_LIST_LIMIT: usize = 50;

/// Hard ceiling on mailbox name length, in chars.
pub const MAX_MAILBOX_CHARS: usize = 100;

/// SMTP connection timeout ceiling (and default), in ms.
pub const MAX_TIMEOUT_MS: u64 = 30_000;

/// Operator-supplied allowlist of env var names a caller's `credentials_env`
/// may name. Comma-separated, exact (case-sensitive) match. Default-deny:
/// unset or empty means no `credentials_env` value is accepted — a caller
/// could otherwise name *any* environment variable in the `email` process (an
/// unrelated secret, not just the SMTP password) and have its value handed to
/// the SMTP client. Same rationale as `search`/`ai`/`tts`/`stt`.
pub const ALLOWED_CRED_ENVS_ENV: &str = "EMAIL_PLUGIN_ALLOWED_CRED_ENVS";

/// Parse [`ALLOWED_CRED_ENVS_ENV`]'s raw value into the set of permitted
/// `credentials_env` names.
pub fn parse_allowed_cred_envs(raw: &str) -> std::collections::HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// True if `name` is permitted as a `credentials_env` value, per the
/// operator's [`ALLOWED_CRED_ENVS_ENV`] allowlist.
pub fn is_allowed_cred_env(name: &str, allowed: &std::collections::HashSet<String>) -> bool {
    allowed.contains(name)
}

/// Minimal address sanity check: must contain an `@` and a `.` after the `@`.
/// Deliberately not RFC 5322 — the actual build is re-validated by `lettre`'s
/// `Address` parser in the handler; this only rejects obviously-broken values
/// at parse time so the error names the offending field.
pub fn is_valid_email(s: &str) -> bool {
    match s.find('@') {
        Some(at) => s[at + 1..].contains('.'),
        None => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailSendParams {
    pub to: String,
    pub from: String,
    pub subject: String,
    pub body: String,
    pub is_html: bool,
    /// Name of an env var (or vault secret) the `email` process reads at call
    /// time. Never a literal password.
    pub credentials_env: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_user: String,
    pub timeout_ms: u64,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Option<String>,
}

/// Parse and validate `params_json` for the `email_send` action. Returns a
/// human-readable error message on any validation failure — caller maps that
/// straight into `ActionResponse.error`.
pub fn parse_request(params_json: &[u8]) -> Result<EmailSendParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        to: Option<String>,
        from: Option<String>,
        subject: Option<String>,
        body: Option<String>,
        is_html: Option<bool>,
        credentials_env: Option<String>,
        smtp_host: Option<String>,
        smtp_port: Option<u16>,
        smtp_user: Option<String>,
        timeout_ms: Option<u64>,
        cc: Option<Vec<String>>,
        bcc: Option<Vec<String>>,
        reply_to: Option<String>,
    }

    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let to = raw.to.ok_or("missing required field: to")?;
    let to = to.trim().to_string();
    if !is_valid_email(&to) {
        return Err(format!("invalid recipient address: '{to}'"));
    }

    let from = match raw.from {
        Some(f) if !f.trim().is_empty() => {
            let f = f.trim().to_string();
            if !is_valid_email(&f) {
                return Err(format!("invalid from address: '{f}'"));
            }
            f
        }
        _ => DEFAULT_FROM.to_string(),
    };

    let subject = raw.subject.ok_or("missing required field: subject")?;
    let subject = subject.trim().to_string();
    let subject_chars = subject.chars().count();
    if subject_chars == 0 || subject_chars > MAX_SUBJECT_CHARS {
        return Err(format!(
            "subject must be 1-{MAX_SUBJECT_CHARS} chars (got {subject_chars})"
        ));
    }

    let body = raw.body.ok_or("missing required field: body")?;
    if body.is_empty() {
        return Err("body must not be empty".to_string());
    }
    let body_chars = body.chars().count();
    if body_chars > MAX_BODY_CHARS {
        return Err(format!(
            "body exceeds max length of {MAX_BODY_CHARS} chars (got {body_chars})"
        ));
    }

    let credentials_env = raw
        .credentials_env
        .ok_or("missing required field: credentials_env")?;
    if credentials_env.is_empty() {
        return Err("credentials_env must not be empty".to_string());
    }

    let smtp_host = match raw.smtp_host {
        Some(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => DEFAULT_SMTP_HOST.to_string(),
    };

    let smtp_port = raw.smtp_port.unwrap_or(DEFAULT_SMTP_PORT);
    if smtp_port == 0 {
        return Err("smtp_port must be non-zero".to_string());
    }

    let smtp_user = match raw.smtp_user {
        Some(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => from.clone(),
    };

    let timeout_ms = raw.timeout_ms.unwrap_or(MAX_TIMEOUT_MS).min(MAX_TIMEOUT_MS);

    let cc = raw.cc.unwrap_or_default();
    for addr in &cc {
        if !is_valid_email(addr) {
            return Err(format!("invalid cc address: '{addr}'"));
        }
    }

    let bcc = raw.bcc.unwrap_or_default();
    for addr in &bcc {
        if !is_valid_email(addr) {
            return Err(format!("invalid bcc address: '{addr}'"));
        }
    }

    let reply_to = match raw.reply_to {
        Some(r) if !r.trim().is_empty() => {
            let r = r.trim().to_string();
            if !is_valid_email(&r) {
                return Err(format!("invalid reply_to address: '{r}'"));
            }
            Some(r)
        }
        _ => None,
    };

    Ok(EmailSendParams {
        to,
        from,
        subject,
        body,
        is_html: raw.is_html.unwrap_or(false),
        credentials_env,
        smtp_host,
        smtp_port,
        smtp_user,
        timeout_ms,
        cc,
        bcc,
        reply_to,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailListParams {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_user: String,
    pub credentials_env: String,
    pub mailbox: String,
    pub limit: usize,
    pub timeout_ms: u64,
}

pub fn parse_email_list_request(params_json: &[u8]) -> Result<EmailListParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        imap_host: Option<String>,
        imap_port: Option<u16>,
        imap_user: Option<String>,
        credentials_env: Option<String>,
        mailbox: Option<String>,
        limit: Option<usize>,
        timeout_ms: Option<u64>,
    }

    let raw: Raw =
        serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let credentials_env = raw
        .credentials_env
        .ok_or("missing required field: credentials_env")?;
    if credentials_env.is_empty() {
        return Err("credentials_env must not be empty".to_string());
    }

    let imap_user = raw
        .imap_user
        .ok_or("missing required field: imap_user")?;
    let imap_user = imap_user.trim().to_string();
    if imap_user.is_empty() {
        return Err("imap_user must not be empty".to_string());
    }

    let imap_host = match raw.imap_host {
        Some(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => DEFAULT_IMAP_HOST.to_string(),
    };

    let imap_port = raw.imap_port.unwrap_or(DEFAULT_IMAP_PORT);
    if imap_port == 0 {
        return Err("imap_port must be non-zero".to_string());
    }

    let mailbox = match raw.mailbox {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => DEFAULT_MAILBOX.to_string(),
    };
    let mailbox_chars = mailbox.chars().count();
    if mailbox_chars == 0 || mailbox_chars > MAX_MAILBOX_CHARS {
        return Err(format!(
            "mailbox must be 1-{MAX_MAILBOX_CHARS} chars (got {mailbox_chars})"
        ));
    }
    if mailbox.contains("..") || mailbox.contains('/') || mailbox.contains('\\') {
        return Err("mailbox must not contain '..', '/' or '\\'".to_string());
    }

    let limit = raw.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(format!(
            "limit must be 1-{MAX_LIST_LIMIT} (got {limit})"
        ));
    }

    let timeout_ms = raw.timeout_ms.unwrap_or(MAX_TIMEOUT_MS).min(MAX_TIMEOUT_MS);

    Ok(EmailListParams {
        imap_host,
        imap_port,
        imap_user,
        credentials_env,
        mailbox,
        limit,
        timeout_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_json() -> serde_json::Value {
        serde_json::json!({
            "to": "user@example.com",
            "subject": "Hello",
            "body": "Hello there",
            "credentials_env": "EMAIL_SMTP_PASS",
        })
    }

    #[test]
    fn accepts_minimal_request_with_defaults() {
        let params = parse_request(valid_json().to_string().as_bytes()).unwrap();
        assert_eq!(params.to, "user@example.com");
        assert_eq!(params.from, DEFAULT_FROM);
        assert_eq!(params.subject, "Hello");
        assert_eq!(params.body, "Hello there");
        assert!(!params.is_html);
        assert_eq!(params.credentials_env, "EMAIL_SMTP_PASS");
        assert_eq!(params.smtp_host, DEFAULT_SMTP_HOST);
        assert_eq!(params.smtp_port, DEFAULT_SMTP_PORT);
        assert_eq!(params.smtp_user, DEFAULT_FROM);
        assert_eq!(params.timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn parses_optional_fields() {
        let mut body = valid_json();
        body["from"] = "sender@example.com".into();
        body["is_html"] = true.into();
        body["smtp_host"] = "smtp.example.com".into();
        body["smtp_port"] = 465.into();
        body["smtp_user"] = "smtp-login".into();
        body["timeout_ms"] = 5000.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.from, "sender@example.com");
        assert!(params.is_html);
        assert_eq!(params.smtp_host, "smtp.example.com");
        assert_eq!(params.smtp_port, 465);
        assert_eq!(params.smtp_user, "smtp-login");
        assert_eq!(params.timeout_ms, 5000);
    }

    #[test]
    fn defaults_smtp_user_to_from_address() {
        let mut body = valid_json();
        body["from"] = "sender@example.com".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.smtp_user, "sender@example.com");
    }

    #[test]
    fn rejects_missing_to() {
        let mut body = valid_json();
        body.as_object_mut().unwrap().remove("to");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("to"), "error was: {err}");
    }

    #[test]
    fn rejects_invalid_email_missing_at() {
        let mut body = valid_json();
        body["to"] = "user.example.com".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("invalid recipient"), "error was: {err}");
    }

    #[test]
    fn rejects_invalid_email_missing_dot_after_at() {
        let mut body = valid_json();
        body["to"] = "user@examplecom".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("invalid recipient"), "error was: {err}");
    }

    #[test]
    fn rejects_invalid_from_address() {
        let mut body = valid_json();
        body["from"] = "not-an-email".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("invalid from"), "error was: {err}");
    }

    #[test]
    fn rejects_missing_subject() {
        let mut body = valid_json();
        body.as_object_mut().unwrap().remove("subject");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("subject"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_subject() {
        let mut body = valid_json();
        body["subject"] = "   ".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("subject"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_subject() {
        let mut body = valid_json();
        body["subject"] = "x".repeat(MAX_SUBJECT_CHARS + 1).into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("subject"), "error was: {err}");
    }

    #[test]
    fn rejects_missing_body() {
        let mut body = valid_json();
        body.as_object_mut().unwrap().remove("body");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("body"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_body() {
        let mut body = valid_json();
        body["body"] = "".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("body"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_body() {
        let mut body = valid_json();
        body["body"] = "x".repeat(MAX_BODY_CHARS + 1).into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("max length"), "error was: {err}");
    }

    #[test]
    fn rejects_missing_credentials_env() {
        let mut body = valid_json();
        body.as_object_mut().unwrap().remove("credentials_env");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("credentials_env"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_credentials_env() {
        let mut body = valid_json();
        body["credentials_env"] = "".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("credentials_env"), "error was: {err}");
    }

    #[test]
    fn rejects_zero_smtp_port() {
        let mut body = valid_json();
        body["smtp_port"] = 0.into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("smtp_port"), "error was: {err}");
    }

    #[test]
    fn accepts_cc_bcc_reply_to() {
        let mut body = valid_json();
        body["cc"] = serde_json::json!(["cc1@example.com", "cc2@example.com"]);
        body["bcc"] = serde_json::json!(["bcc@example.com"]);
        body["reply_to"] = "reply@example.com".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.cc, vec!["cc1@example.com", "cc2@example.com"]);
        assert_eq!(params.bcc, vec!["bcc@example.com"]);
        assert_eq!(params.reply_to, Some("reply@example.com".to_string()));
    }

    #[test]
    fn defaults_cc_bcc_reply_to_when_omitted() {
        let params = parse_request(valid_json().to_string().as_bytes()).unwrap();
        assert!(params.cc.is_empty());
        assert!(params.bcc.is_empty());
        assert_eq!(params.reply_to, None);
    }

    #[test]
    fn rejects_invalid_cc_address() {
        let mut body = valid_json();
        body["cc"] = serde_json::json!(["not-an-email"]);
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("cc"), "error was: {err}");
    }

    #[test]
    fn rejects_invalid_bcc_address() {
        let mut body = valid_json();
        body["bcc"] = serde_json::json!(["not-an-email"]);
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("bcc"), "error was: {err}");
    }

    #[test]
    fn rejects_invalid_reply_to_address() {
        let mut body = valid_json();
        body["reply_to"] = "not-an-email".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("reply_to"), "error was: {err}");
    }

    #[test]
    fn clamps_timeout_ms_above_cap() {
        let mut body = valid_json();
        body["timeout_ms"] = 999_999.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn allowed_cred_envs_empty_by_default() {
        assert!(parse_allowed_cred_envs("").is_empty());
    }

    #[test]
    fn allowed_cred_envs_parses_comma_list() {
        let allowed = parse_allowed_cred_envs("EMAIL_SMTP_PASS, EMAIL_SMTP_PASS_ALT ,,");
        assert!(is_allowed_cred_env("EMAIL_SMTP_PASS", &allowed));
        assert!(is_allowed_cred_env("EMAIL_SMTP_PASS_ALT", &allowed));
        assert_eq!(allowed.len(), 2);
    }

    #[test]
    fn is_allowed_cred_env_rejects_unlisted_name() {
        let allowed = parse_allowed_cred_envs("EMAIL_SMTP_PASS");
        assert!(!is_allowed_cred_env("AWS_SECRET_ACCESS_KEY", &allowed));
    }

    #[test]
    fn is_allowed_cred_env_is_case_sensitive() {
        let allowed = parse_allowed_cred_envs("EMAIL_SMTP_PASS");
        assert!(!is_allowed_cred_env("email_smtp_pass", &allowed));
    }

    #[test]
    fn is_allowed_cred_env_rejects_everything_when_empty() {
        let allowed = parse_allowed_cred_envs("");
        assert!(!is_allowed_cred_env("EMAIL_SMTP_PASS", &allowed));
    }

    fn valid_list_json() -> serde_json::Value {
        serde_json::json!({
            "imap_user": "user@example.com",
            "credentials_env": "EMAIL_SMTP_PASS",
        })
    }

    #[test]
    fn accepts_minimal_list_request_with_defaults() {
        let params = parse_email_list_request(valid_list_json().to_string().as_bytes()).unwrap();
        assert_eq!(params.imap_user, "user@example.com");
        assert_eq!(params.credentials_env, "EMAIL_SMTP_PASS");
        assert_eq!(params.imap_host, DEFAULT_IMAP_HOST);
        assert_eq!(params.imap_port, DEFAULT_IMAP_PORT);
        assert_eq!(params.mailbox, DEFAULT_MAILBOX);
        assert_eq!(params.limit, DEFAULT_LIST_LIMIT);
        assert_eq!(params.timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn parses_list_optional_fields() {
        let mut j = valid_list_json();
        j["imap_host"] = "imap.example.com".into();
        j["imap_port"] = 143.into();
        j["mailbox"] = "Sent".into();
        j["limit"] = 25.into();
        j["timeout_ms"] = 5000.into();
        let params = parse_email_list_request(j.to_string().as_bytes()).unwrap();
        assert_eq!(params.imap_host, "imap.example.com");
        assert_eq!(params.imap_port, 143);
        assert_eq!(params.mailbox, "Sent");
        assert_eq!(params.limit, 25);
        assert_eq!(params.timeout_ms, 5000);
    }

    #[test]
    fn rejects_list_missing_imap_user() {
        let mut j = valid_list_json();
        j.as_object_mut().unwrap().remove("imap_user");
        let err = parse_email_list_request(j.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("imap_user"), "error was: {err}");
    }

    #[test]
    fn rejects_list_missing_credentials_env() {
        let mut j = valid_list_json();
        j.as_object_mut().unwrap().remove("credentials_env");
        let err = parse_email_list_request(j.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("credentials_env"), "error was: {err}");
    }

    #[test]
    fn rejects_list_invalid_mailbox_traversal() {
        let mut j = valid_list_json();
        j["mailbox"] = "../INBOX".into();
        let err = parse_email_list_request(j.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("mailbox"), "error was: {err}");
    }

    #[test]
    fn rejects_list_zero_limit() {
        let mut j = valid_list_json();
        j["limit"] = 0.into();
        let err = parse_email_list_request(j.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("limit"), "error was: {err}");
    }

    #[test]
    fn rejects_list_limit_above_max() {
        let mut j = valid_list_json();
        j["limit"] = (MAX_LIST_LIMIT + 1).into();
        let err = parse_email_list_request(j.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("limit"), "error was: {err}");
    }

    #[test]
    fn rejects_list_zero_imap_port() {
        let mut j = valid_list_json();
        j["imap_port"] = 0.into();
        let err = parse_email_list_request(j.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("imap_port"), "error was: {err}");
    }
}
