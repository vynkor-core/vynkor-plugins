//! Parse + validate the JSON body of a `web_search` `ActionRequest`.

/// Hard ceiling on the query length, in chars. Bounds the URL/body the plugin
/// hands to `network` and matches provider-side input limits.
pub const MAX_QUERY_CHARS: usize = 400;

/// Default result count when the caller omits `count`.
pub const DEFAULT_COUNT: u32 = 5;

/// Hard ceiling on `count`. Clamped, never rejected (Brave caps at 20 too).
pub const MAX_COUNT: u32 = 20;

/// Action-level timeout ceiling. Matches `network`'s own `http_request` cap,
/// so a `web_search` call can't outlive the HTTP request it wraps.
pub const MAX_TIMEOUT_MS: u64 = 30_000;

pub const DEFAULT_BRAVE_BASE_URL: &str = "https://api.search.brave.com";
pub const DEFAULT_TAVILY_BASE_URL: &str = "https://api.tavily.com";

/// Operator-supplied allowlist of env var names a caller's `api_key_env`
/// may name. Comma-separated, exact (case-sensitive) match. Default-deny:
/// unset or empty means no `api_key_env` value is accepted — a caller could
/// otherwise name *any* environment variable in the `search` process (an
/// unrelated secret, not just a provider key) and have its value sent
/// straight into an outbound request header to a caller-controlled
/// `base_url`, exfiltrating it. Same rationale as `ai`/`tts`/`stt`.
pub const ALLOWED_KEY_ENVS_ENV: &str = "SEARCH_PLUGIN_ALLOWED_KEY_ENVS";

/// Parse [`ALLOWED_KEY_ENVS_ENV`]'s raw value into the set of permitted
/// `api_key_env` names.
pub fn parse_allowed_key_envs(raw: &str) -> std::collections::HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// True if `name` is permitted as an `api_key_env` value, per the operator's
/// [`ALLOWED_KEY_ENVS_ENV`] allowlist.
pub fn is_allowed_key_env(name: &str, allowed: &std::collections::HashSet<String>) -> bool {
    allowed.contains(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Brave,
    Tavily,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Brave => "brave",
            Provider::Tavily => "tavily",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchParams {
    pub query: String,
    pub provider: Provider,
    /// Name of an env var (or vault secret) the `search` process reads at
    /// call time. Never a literal key.
    pub api_key_env: String,
    pub base_url: String,
    pub count: u32,
    pub timeout_ms: u64,
}

/// Parse and validate `params_json` for the `web_search` action. Returns a
/// human-readable error message on any validation failure — caller maps that
/// straight into `ActionResponse.error`.
pub fn parse_request(params_json: &[u8]) -> Result<WebSearchParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        query: Option<String>,
        provider: Option<String>,
        api_key_env: Option<String>,
        base_url: Option<String>,
        count: Option<u32>,
        timeout_ms: Option<u64>,
    }

    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let query = raw.query.ok_or("missing required field: query")?;
    let query = query.trim().to_string();
    if query.is_empty() {
        return Err("query must not be empty".to_string());
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(format!(
            "query exceeds max length of {MAX_QUERY_CHARS} chars (got {})",
            query.chars().count()
        ));
    }

    let provider = match raw.provider.as_deref() {
        None | Some("brave") => Provider::Brave,
        Some("tavily") => Provider::Tavily,
        Some(other) => return Err(format!("unsupported provider: {other}")),
    };

    let api_key_env = raw
        .api_key_env
        .ok_or("missing required field: api_key_env")?;
    if api_key_env.is_empty() {
        return Err("api_key_env must not be empty".to_string());
    }

    let base_url = match raw.base_url {
        Some(u) if !u.is_empty() => u,
        _ => match provider {
            Provider::Brave => DEFAULT_BRAVE_BASE_URL.to_string(),
            Provider::Tavily => DEFAULT_TAVILY_BASE_URL.to_string(),
        },
    };

    let count = raw.count.unwrap_or(DEFAULT_COUNT).min(MAX_COUNT);
    let timeout_ms = raw.timeout_ms.unwrap_or(MAX_TIMEOUT_MS).min(MAX_TIMEOUT_MS);

    Ok(WebSearchParams {
        query,
        provider,
        api_key_env,
        base_url,
        count,
        timeout_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_json() -> serde_json::Value {
        serde_json::json!({
            "query": "vynkor plugin kernel",
            "api_key_env": "SEARCH_BRAVE_KEY",
        })
    }

    #[test]
    fn accepts_minimal_request_defaults_to_brave() {
        let params = parse_request(valid_json().to_string().as_bytes()).unwrap();
        assert_eq!(params.provider, Provider::Brave);
        assert_eq!(params.base_url, DEFAULT_BRAVE_BASE_URL);
        assert_eq!(params.count, DEFAULT_COUNT);
        assert_eq!(params.timeout_ms, MAX_TIMEOUT_MS);
        assert_eq!(params.query, "vynkor plugin kernel");
    }

    #[test]
    fn selects_tavily_provider() {
        let mut body = valid_json();
        body["provider"] = "tavily".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.provider, Provider::Tavily);
        assert_eq!(params.base_url, DEFAULT_TAVILY_BASE_URL);
    }

    #[test]
    fn rejects_missing_query() {
        let mut body = valid_json();
        body.as_object_mut().unwrap().remove("query");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("query"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_query() {
        let mut body = valid_json();
        body["query"] = "   ".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("query"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_query() {
        let mut body = valid_json();
        body["query"] = "x".repeat(MAX_QUERY_CHARS + 1).into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("max length"), "error was: {err}");
    }

    #[test]
    fn rejects_unsupported_provider() {
        let mut body = valid_json();
        body["provider"] = "google".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("unsupported provider"), "error was: {err}");
    }

    #[test]
    fn rejects_missing_api_key_env() {
        let mut body = valid_json();
        body.as_object_mut().unwrap().remove("api_key_env");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("api_key_env"), "error was: {err}");
    }

    #[test]
    fn accepts_explicit_base_url() {
        let mut body = valid_json();
        body["base_url"] = "http://localhost:8080".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.base_url, "http://localhost:8080");
    }

    #[test]
    fn clamps_count_above_cap() {
        let mut body = valid_json();
        body["count"] = 99.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.count, MAX_COUNT);
    }

    #[test]
    fn clamps_timeout_ms_above_cap() {
        let mut body = valid_json();
        body["timeout_ms"] = 999_999.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn allowed_key_envs_empty_by_default() {
        assert!(parse_allowed_key_envs("").is_empty());
    }

    #[test]
    fn allowed_key_envs_parses_comma_list() {
        let allowed = parse_allowed_key_envs("SEARCH_BRAVE_KEY, SEARCH_TAVILY_KEY ,,");
        assert!(is_allowed_key_env("SEARCH_BRAVE_KEY", &allowed));
        assert!(is_allowed_key_env("SEARCH_TAVILY_KEY", &allowed));
        assert_eq!(allowed.len(), 2);
    }

    #[test]
    fn is_allowed_key_env_rejects_unlisted_name() {
        let allowed = parse_allowed_key_envs("SEARCH_BRAVE_KEY");
        assert!(!is_allowed_key_env("AWS_SECRET_ACCESS_KEY", &allowed));
    }

    #[test]
    fn is_allowed_key_env_is_case_sensitive() {
        let allowed = parse_allowed_key_envs("SEARCH_BRAVE_KEY");
        assert!(!is_allowed_key_env("search_brave_key", &allowed));
    }

    #[test]
    fn is_allowed_key_env_rejects_everything_when_empty() {
        let allowed = parse_allowed_key_envs("");
        assert!(!is_allowed_key_env("SEARCH_BRAVE_KEY", &allowed));
    }
}
