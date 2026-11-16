//! Parse + validate the JSON body of an `http_request` `ActionRequest`.

use std::collections::HashMap;

/// `timeout_ms` when the caller names none; matches the kernel's default
/// action timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling on `timeout_ms`. Above the default because a local CPU
/// model behind `ai` routinely needs minutes per completion; a caller that
/// asks for more than this is clamped down, never rejected.
pub const MAX_TIMEOUT_MS: u64 = 300_000;

/// Hard ceiling on `max_retries`. Retries are opt-in per request — a caller
/// that doesn't set `max_retries` gets none, so an unmodified caller sees no
/// behavior change.
pub const MAX_RETRIES: u32 = 5;

/// Default initial backoff between retry attempts, used when the caller
/// omits `retry_backoff_ms`.
pub const DEFAULT_RETRY_BACKOFF_MS: u64 = 200;

/// Hard ceiling on `retry_backoff_ms` (also caps the exponential growth
/// between attempts), so a caller can't turn a retry into a multi-minute
/// stall.
pub const MAX_RETRY_BACKOFF_MS: u64 = 5_000;

/// Hard ceiling on URL length. Rejected outright, never truncated.
pub const MAX_URL_LEN: usize = 8 * 1024;

/// Hard ceiling on header count. Rejected outright.
pub const MAX_HEADER_COUNT: usize = 100;

/// Hard ceiling on total header bytes (sum of every key+value length).
/// Rejected outright — this bounds worst-case memory for a request with
/// many small headers as well as a few huge ones.
pub const MAX_HEADERS_TOTAL_BYTES: usize = 32 * 1024;

/// Hard ceiling on the decoded size of a `body_base64` request body.
/// Matches OpenAI's audio upload cap — the largest legitimate binary body
/// a plugin needs to push through `network` today (a Whisper-style
/// multipart upload). Rejected outright.
pub const MAX_REQUEST_BODY_BYTES: usize = 25 * 1024 * 1024;

/// Redirects are disabled unless `follow_redirects` is set, and even then
/// capped at this many hops. `max_redirects` is caller-configurable per
/// request (defaults to this, clamped to it) — see main.rs, which keeps one
/// redirect-enabled client per distinct cap so per-request limits don't
/// forfeit connection pooling. Every hop still resolves through
/// `SsrfSafeResolver`.
pub const MAX_REDIRECTS: usize = 10;

/// Hard ceiling on a multipart part's `name` field, in bytes. Rejected
/// outright.
pub const MAX_MULTIPART_NAME_BYTES: usize = 256;

/// Hard ceiling on a multipart part's `filename` field, in bytes. Rejected
/// outright.
pub const MAX_MULTIPART_FILENAME_BYTES: usize = 1024;

/// One `multipart/form-data` part. Exactly one of `value` (text) or
/// `file_base64` (binary) is required; `filename` and `content_type` are
/// optional presentation hints.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct MultipartPart {
    /// Part field name, `1..=MAX_MULTIPART_NAME_BYTES` bytes.
    pub name: String,
    /// Text part value (UTF-8). Mutually exclusive with `file_base64`.
    #[serde(default)]
    pub value: Option<String>,
    /// Base64-encoded binary part content. Mutually exclusive with `value`.
    #[serde(default)]
    pub file_base64: Option<String>,
    /// `filename` in the part's `Content-Disposition` (file uploads).
    #[serde(default)]
    pub filename: Option<String>,
    /// Part `Content-Type`. Defaults to `text/plain; charset=utf-8` for
    /// `value` parts and `application/octet-stream` for `file_base64` parts.
    #[serde(default)]
    pub content_type: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct HttpRequestParams {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    /// Base64-encoded binary request body, mutually exclusive with `body`
    /// (which is UTF-8 only and would mangle binary bytes). Set the
    /// `Content-Type` header yourself when sending one.
    pub body_base64: Option<String>,
    /// `multipart/form-data` body parts, mutually exclusive with both `body`
    /// and `body_base64`. The plugin builds the boundary and overrides the
    /// `Content-Type` header.
    pub multipart: Option<Vec<MultipartPart>>,
    /// Serve repeat requests from the per-caller in-memory cache for this
    /// many ms. `0` (default) disables caching. A fresh hit returns the
    /// stored 2xx response without touching the network; only 2xx responses
    /// are ever stored, and `Cache-Control: no-store` responses are never
    /// cached.
    pub cache_ttl_ms: u64,
    /// Keep a per-caller in-memory session cookie jar for this request:
    /// `Set-Cookie` headers on the response update it, and the matching
    /// cookies are attached to later requests to the same host. Caller's
    /// own `Cookie` header wins over the jar. Jar is host-scoped, has no
    /// expiry/domain/path matching, and is cleared when the plugin restarts.
    pub use_cookies: bool,
    pub timeout_ms: u64,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
    pub follow_redirects: bool,
    /// Max redirect hops to follow when `follow_redirects` is true. Defaults
    /// to `MAX_REDIRECTS`, clamped to it. Ignored when
    /// `follow_redirects` is false.
    pub max_redirects: usize,
}

const ALLOWED_METHODS: &[&str] = &[
    "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS",
];

/// Parse and validate `params_json` for the `http_request` action.
/// Returns a human-readable error message on any validation failure —
/// caller maps that straight into `ActionResponse.error`.
pub fn parse_request(params_json: &[u8]) -> Result<HttpRequestParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        method: Option<String>,
        url: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
        body: Option<String>,
        body_base64: Option<String>,
        #[serde(default)]
        multipart: Option<Vec<MultipartPart>>,
        cache_ttl_ms: Option<u64>,
        use_cookies: Option<bool>,
        timeout_ms: Option<u64>,
        max_retries: Option<u32>,
        retry_backoff_ms: Option<u64>,
        follow_redirects: Option<bool>,
        max_redirects: Option<usize>,
    }

    let raw: Raw =
        serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let method = raw.method.ok_or("missing required field: method")?;
    let method = method.to_uppercase();
    if !ALLOWED_METHODS.contains(&method.as_str()) {
        return Err(format!("unsupported method: {method}"));
    }

    let url_str = raw.url.ok_or("missing required field: url")?;
    if url_str.len() > MAX_URL_LEN {
        return Err(format!("url exceeds {MAX_URL_LEN}-byte cap"));
    }
    let parsed = url::Url::parse(&url_str).map_err(|e| format!("invalid url: {e}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(format!("blocked scheme: {}", parsed.scheme()));
    }

    if raw.headers.len() > MAX_HEADER_COUNT {
        return Err(format!("too many headers: max {MAX_HEADER_COUNT}"));
    }
    let headers_total_bytes: usize = raw.headers.iter().map(|(k, v)| k.len() + v.len()).sum();
    if headers_total_bytes > MAX_HEADERS_TOTAL_BYTES {
        return Err(format!(
            "headers exceed {MAX_HEADERS_TOTAL_BYTES}-byte total cap"
        ));
    }

    if raw.body.is_some() && raw.body_base64.is_some() {
        return Err("set body or body_base64, not both".to_string());
    }
    if raw.multipart.is_some() && (raw.body.is_some() || raw.body_base64.is_some()) {
        return Err("multipart is mutually exclusive with body and body_base64".to_string());
    }
    if let Some(parts) = &raw.multipart {
        validate_multipart(parts)?;
    }
    if let Some(b64) = &raw.body_base64 {
        // Base64 of exactly MAX_REQUEST_BODY_BYTES bytes is at most
        // 4 * ceil(n / 3) chars; reject anything larger up front so a
        // huge base64 blob can't be decoded (and buffered) in vain.
        if b64.len() > (MAX_REQUEST_BODY_BYTES / 3) * 4 + 4 {
            return Err(format!(
                "body_base64 decodes to more than the {MAX_REQUEST_BODY_BYTES}-byte cap"
            ));
        }
    }

    let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
    let max_retries = raw.max_retries.unwrap_or(0).min(MAX_RETRIES);
    let retry_backoff_ms = raw
        .retry_backoff_ms
        .unwrap_or(DEFAULT_RETRY_BACKOFF_MS)
        .min(MAX_RETRY_BACKOFF_MS);

    Ok(HttpRequestParams {
        method,
        url: url_str,
        headers: raw.headers,
        body: raw.body,
        body_base64: raw.body_base64,
        multipart: raw.multipart,
        cache_ttl_ms: raw.cache_ttl_ms.unwrap_or(0),
        use_cookies: raw.use_cookies.unwrap_or(false),
        timeout_ms,
        max_retries,
        retry_backoff_ms,
        follow_redirects: raw.follow_redirects.unwrap_or(false),
        max_redirects: raw.max_redirects.unwrap_or(MAX_REDIRECTS).min(MAX_REDIRECTS),
    })
}

/// Validate a multipart part list: caps, value/file_base64 exclusivity, and
/// a decoded-size estimate against [`MAX_REQUEST_BODY_BYTES`]. Rejects, never
/// truncates — same policy as every other cap in this file.
fn validate_multipart(parts: &[MultipartPart]) -> Result<(), String> {
    let mut total_estimate: usize = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.name.is_empty() || part.name.len() > MAX_MULTIPART_NAME_BYTES {
            return Err(format!(
                "multipart part {i} name must be 1..={MAX_MULTIPART_NAME_BYTES} bytes"
            ));
        }
        if let Some(f) = &part.filename {
            if f.len() > MAX_MULTIPART_FILENAME_BYTES {
                return Err(format!(
                    "multipart part {i} filename exceeds {MAX_MULTIPART_FILENAME_BYTES}-byte cap"
                ));
            }
        }
        match (&part.value, &part.file_base64) {
            (Some(v), None) => total_estimate = total_estimate.saturating_add(v.len()),
            (None, Some(b64)) => {
                let estimate = b64.len().saturating_mul(3) / 4;
                if estimate > MAX_REQUEST_BODY_BYTES {
                    return Err(format!(
                        "multipart part {i} decodes to more than the {MAX_REQUEST_BODY_BYTES}-byte cap"
                    ));
                }
                total_estimate = total_estimate.saturating_add(estimate);
            }
            (Some(_), Some(_)) => {
                return Err(format!(
                    "multipart part {i} has both value and file_base64 — pick one"
                ))
            }
            (None, None) => {
                return Err(format!(
                    "multipart part {i} needs exactly one of value or file_base64"
                ))
            }
        }
    }
    if total_estimate > MAX_REQUEST_BODY_BYTES {
        return Err(format!(
            "multipart body decodes to more than the {MAX_REQUEST_BODY_BYTES}-byte cap"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_url() {
        let err = parse_request(br#"{"method": "GET"}"#).unwrap_err();
        assert!(err.contains("url"), "error was: {err}");
    }

    #[test]
    fn rejects_bad_scheme() {
        let err = parse_request(br#"{"method": "GET", "url": "file:///etc/passwd"}"#)
            .unwrap_err();
        assert!(err.contains("scheme"), "error was: {err}");
    }

    #[test]
    fn rejects_bad_method() {
        let err =
            parse_request(br#"{"method": "TRACE", "url": "https://example.com"}"#).unwrap_err();
        assert!(err.contains("method"), "error was: {err}");
    }

    #[test]
    fn accepts_minimal_valid_request() {
        let params =
            parse_request(br#"{"method": "get", "url": "https://example.com/thing"}"#).unwrap();
        assert_eq!(params.method, "GET");
        assert_eq!(params.url, "https://example.com/thing");
        assert_eq!(params.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(params.body.is_none());
    }

    #[test]
    fn clamps_timeout_above_cap() {
        let params = parse_request(
            br#"{"method": "GET", "url": "https://example.com", "timeout_ms": 999999}"#,
        )
        .unwrap();
        assert_eq!(params.timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn honors_timeout_between_default_and_cap() {
        let params = parse_request(
            br#"{"method": "GET", "url": "https://example.com", "timeout_ms": 180000}"#,
        )
        .unwrap();
        assert_eq!(params.timeout_ms, 180_000);
    }

    #[test]
    fn preserves_timeout_below_cap() {
        let params = parse_request(
            br#"{"method": "GET", "url": "https://example.com", "timeout_ms": 500}"#,
        )
        .unwrap();
        assert_eq!(params.timeout_ms, 500);
    }

    #[test]
    fn accepts_body_base64_alone() {
        let params = parse_request(br#"{
            "method": "POST",
            "url": "https://example.com/upload",
            "headers": {"Content-Type": "application/octet-stream"},
            "body_base64": "AAEC/w=="
        }"#)
        .unwrap();
        assert!(params.body.is_none());
        assert_eq!(params.body_base64.as_deref(), Some("AAEC/w=="));
    }

    #[test]
    fn rejects_both_body_and_body_base64() {
        let err = parse_request(br#"{
            "method": "POST",
            "url": "https://example.com/upload",
            "body": "text",
            "body_base64": "AAEC/w=="
        }"#)
        .unwrap_err();
        assert!(err.contains("not both"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_body_base64() {
        let huge_b64 = "A".repeat((MAX_REQUEST_BODY_BYTES / 3) * 4 + 8);
        let body = serde_json::json!({
            "method": "POST",
            "url": "https://example.com/upload",
            "body_base64": huge_b64,
        })
        .to_string();
        let err = parse_request(body.as_bytes()).unwrap_err();
        assert!(err.contains("cap"), "error was: {err}");
    }

    #[test]
    fn defaults_to_no_retries() {
        let params =
            parse_request(br#"{"method": "GET", "url": "https://example.com"}"#).unwrap();
        assert_eq!(params.max_retries, 0);
        assert_eq!(params.retry_backoff_ms, DEFAULT_RETRY_BACKOFF_MS);
    }

    #[test]
    fn clamps_max_retries_above_cap() {
        let params = parse_request(
            br#"{"method": "GET", "url": "https://example.com", "max_retries": 99}"#,
        )
        .unwrap();
        assert_eq!(params.max_retries, MAX_RETRIES);
    }

    #[test]
    fn rejects_url_over_length_cap() {
        let long_path = "a".repeat(MAX_URL_LEN);
        let url = format!("https://example.com/{long_path}");
        let body = serde_json::json!({"method": "GET", "url": url}).to_string();
        let err = parse_request(body.as_bytes()).unwrap_err();
        assert!(err.contains("url"), "error was: {err}");
    }

    #[test]
    fn rejects_too_many_headers() {
        let headers: HashMap<String, String> = (0..MAX_HEADER_COUNT + 1)
            .map(|i| (format!("h{i}"), "v".to_string()))
            .collect();
        let body = serde_json::json!({
            "method": "GET",
            "url": "https://example.com",
            "headers": headers,
        })
        .to_string();
        let err = parse_request(body.as_bytes()).unwrap_err();
        assert!(err.contains("too many headers"), "error was: {err}");
    }

    #[test]
    fn rejects_headers_over_total_byte_cap() {
        let mut headers = HashMap::new();
        headers.insert("h".to_string(), "v".repeat(MAX_HEADERS_TOTAL_BYTES + 1));
        let body = serde_json::json!({
            "method": "GET",
            "url": "https://example.com",
            "headers": headers,
        })
        .to_string();
        let err = parse_request(body.as_bytes()).unwrap_err();
        assert!(err.contains("byte total cap"), "error was: {err}");
    }

    #[test]
    fn clamps_retry_backoff_above_cap() {
        let params = parse_request(
            br#"{"method": "GET", "url": "https://example.com", "retry_backoff_ms": 999999}"#,
        )
        .unwrap();
        assert_eq!(params.retry_backoff_ms, MAX_RETRY_BACKOFF_MS);
    }

    #[test]
    fn defaults_max_redirects_to_cap() {
        let params =
            parse_request(br#"{"method": "GET", "url": "https://example.com"}"#).unwrap();
        assert_eq!(params.max_redirects, MAX_REDIRECTS);
    }

    #[test]
    fn preserves_max_redirects_below_cap() {
        let params = parse_request(
            br#"{"method": "GET", "url": "https://example.com", "max_redirects": 2}"#,
        )
        .unwrap();
        assert_eq!(params.max_redirects, 2);
    }

    #[test]
    fn clamps_max_redirects_above_cap() {
        let params = parse_request(
            br#"{"method": "GET", "url": "https://example.com", "max_redirects": 99}"#,
        )
        .unwrap();
        assert_eq!(params.max_redirects, MAX_REDIRECTS);
    }

    #[test]
    fn defaults_cache_and_cookies_off() {
        let params = parse_request(br#"{"method": "GET", "url": "https://example.com"}"#)
            .unwrap();
        assert_eq!(params.cache_ttl_ms, 0);
        assert!(!params.use_cookies);
        assert!(params.multipart.is_none());
    }

    #[test]
    fn parses_multipart_parts() {
        let params = parse_request(br#"{
            "method": "POST",
            "url": "https://example.com/upload",
            "multipart": [
                {"name": "note", "value": "hello"},
                {"name": "file", "file_base64": "AAEC/w==", "filename": "blob.bin", "content_type": "application/octet-stream"}
            ]
        }"#)
        .unwrap();
        let parts = params.multipart.unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "note");
        assert_eq!(parts[0].value.as_deref(), Some("hello"));
        assert_eq!(parts[1].file_base64.as_deref(), Some("AAEC/w=="));
        assert_eq!(parts[1].filename.as_deref(), Some("blob.bin"));
    }

    #[test]
    fn rejects_multipart_with_body_and_body_base64() {
        let err = parse_request(br#"{
            "method": "POST",
            "url": "https://example.com/upload",
            "body": "text",
            "multipart": [{"name": "a", "value": "b"}]
        }"#)
        .unwrap_err();
        assert!(err.contains("multipart is mutually exclusive"), "error was: {err}");

        let err = parse_request(br#"{
            "method": "POST",
            "url": "https://example.com/upload",
            "body_base64": "AAEC/w==",
            "multipart": [{"name": "a", "value": "b"}]
        }"#)
        .unwrap_err();
        assert!(err.contains("multipart is mutually exclusive"), "error was: {err}");
    }

    #[test]
    fn rejects_part_with_both_or_neither_value_and_file() {
        let err = parse_request(br#"{
            "method": "POST",
            "url": "https://example.com/upload",
            "multipart": [{"name": "a", "value": "b", "file_base64": "AA=="}]
        }"#)
        .unwrap_err();
        assert!(err.contains("both value and file_base64"), "error was: {err}");

        let err = parse_request(br#"{
            "method": "POST",
            "url": "https://example.com/upload",
            "multipart": [{"name": "a"}]
        }"#)
        .unwrap_err();
        assert!(err.contains("exactly one of value or file_base64"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_part_name_and_filename() {
        let big_name = "n".repeat(MAX_MULTIPART_NAME_BYTES + 1);
        let body = serde_json::json!({
            "method": "POST",
            "url": "https://example.com/upload",
            "multipart": [{"name": big_name, "value": "v"}],
        })
        .to_string();
        let err = parse_request(body.as_bytes()).unwrap_err();
        assert!(err.contains("name must be"), "error was: {err}");

        let big_file = "f".repeat(MAX_MULTIPART_FILENAME_BYTES + 1);
        let body = serde_json::json!({
            "method": "POST",
            "url": "https://example.com/upload",
            "multipart": [{"name": "a", "value": "v", "filename": big_file}],
        })
        .to_string();
        let err = parse_request(body.as_bytes()).unwrap_err();
        assert!(err.contains("filename exceeds"), "error was: {err}");
    }

    #[test]
    fn rejects_multipart_total_over_body_cap() {
        let big_value = "x".repeat(MAX_REQUEST_BODY_BYTES + 1);
        let body = serde_json::json!({
            "method": "POST",
            "url": "https://example.com/upload",
            "multipart": [{"name": "a", "value": big_value}],
        })
        .to_string();
        let err = parse_request(body.as_bytes()).unwrap_err();
        assert!(err.contains("more than the"), "error was: {err}");
    }

    #[test]
    fn parses_cache_ttl_and_cookies() {
        let params = parse_request(br#"{
            "method": "GET",
            "url": "https://example.com",
            "cache_ttl_ms": 5000,
            "use_cookies": true
        }"#)
        .unwrap();
        assert_eq!(params.cache_ttl_ms, 5000);
        assert!(params.use_cookies);
    }
}
