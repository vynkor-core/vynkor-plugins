//! Executes an already-validated [`HttpRequestParams`] and maps the result
//! into the JSON shape the plugin returns in `ActionResponse.data_json`.
//!
//! SSRF protection lives in [`SsrfSafeResolver`], plugged in as the
//! `reqwest::Client`'s DNS resolver (see `main.rs`) rather than as a
//! pre-flight check. A separate pre-flight resolve-then-connect has a
//! DNS-rebinding TOCTOU: the name can re-resolve to a different (blocked)
//! IP between the check and the actual connect, and it doesn't cover
//! redirects to a new host. Gating at the resolver makes every connection
//! reqwest makes — initial request and any followed redirect — resolve
//! through the same authoritative, filtered lookup. Redirects are also
//! disabled at the client level (`main.rs`) as defense in depth.
//!
//! `SsrfSafeResolver` only runs for hostnames, though — `reqwest`/`hyper`
//! skip DNS resolution entirely when a URL's host is already a literal IP,
//! so it never reaches this resolver at all. `main.rs` covers that gap with
//! an explicit `ssrf::check_literal_ip_host` call, both before the initial
//! request (`handle_http_request`) and on every redirect hop
//! (`redirect_policy`).
//!
//! `fetch` itself has no SSRF gate, so its tests exercise the HTTP-send
//! logic directly against a loopback mock server with a plain
//! `reqwest::Client`, without depending on `ssrf::is_blocked_ip` (left as a
//! TODO for the plugin author) and without loopback being rejected.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::request::HttpRequestParams;
use crate::ssrf::{self, Allowlist, Blocklist};

/// Response bodies larger than this are rejected outright (`ACTION_ERROR`),
/// never silently truncated.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Failure of one HTTP attempt, carrying whether retrying could plausibly
/// help. Deterministic failures (SSRF-policy rejection, response body over
/// the cap) reproduce identically on every attempt, so retrying them just
/// burns the caller's retry budget and `network`'s egress time.
#[derive(Debug)]
struct FetchError {
    message: String,
    retryable: bool,
}

impl FetchError {
    /// Failure that may be transient (connection refused, timeout, 5xx) —
    /// worth retrying.
    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    /// Failure that is deterministic — retrying will produce the same
    /// result, so don't.
    fn deterministic(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct HttpResponseJson {
    pub status: u16,
    pub headers: HashMap<String, String>,
    /// Text body as-is when valid UTF-8, otherwise base64 — see
    /// `body_encoding`. Never lossily mangled: binary responses (images,
    /// protobuf, etc.) round-trip exactly via the base64 path.
    pub body: String,
    /// `"utf8"` or `"base64"`, telling the caller how to interpret `body`.
    pub body_encoding: &'static str,
    /// `"hit"`/`"miss"` when the caller requested caching (`cache_ttl_ms`);
    /// `None` when caching was not requested. Set by the plugin (main.rs),
    /// not by the fetch path itself.
    pub cache: Option<&'static str>,
}

/// DNS resolver that filters out any IP blocked by [`ssrf::is_blocked_ip`].
/// Install via `Client::builder().dns_resolver(...)` so it's the single,
/// authoritative resolution used for both the initial connect and any
/// redirect hop — no separate pre-flight check to go stale.
#[derive(Clone, Default)]
pub struct SsrfSafeResolver {
    pub extra_blocklist: Blocklist,
    /// When non-empty, switches from default-block (built-in ranges) to
    /// default-deny: only hosts/IPs listed here (or in neither, minus
    /// `extra_blocklist`) may be reached — see [`Allowlist`] docs.
    pub allowlist: Allowlist,
}

/// Filter a resolution result down to the addresses SSRF policy permits:
/// dropped if in the operator's extra blocklist, and otherwise allowed only
/// when the allowlist names the host/IP or, absent an allowlist, the
/// address isn't in the built-in blocked ranges.
fn filter_allowed_addrs(
    host: &str,
    addrs: impl Iterator<Item = std::net::SocketAddr>,
    extra_blocklist: &Blocklist,
    allowlist: &Allowlist,
) -> Vec<std::net::SocketAddr> {
    addrs
        .filter(|a| {
            if extra_blocklist.blocks_ip(&a.ip()) {
                return false;
            }
            if !allowlist.is_empty() {
                allowlist.allows_host(host) || allowlist.allows_ip(&a.ip())
            } else {
                !ssrf::is_blocked_ip(a.ip())
            }
        })
        .collect()
}

/// Deterministic pre-check that SSRF policy permits reaching `host` at all.
/// Not the authoritative gate for a real request — a name can re-resolve
/// between this check and the connect (rebinding TOCTOU), so
/// [`SsrfSafeResolver`] stays the authority and re-filters every actual
/// connect. This exists so callers can fail fast (and outside the retry
/// loop) on a request that was never going anywhere, mirroring the
/// literal-IP gate `ssrf::check_literal_ip_host` provides for IP hosts.
pub async fn check_host_reachable(
    host: &str,
    extra_blocklist: &Blocklist,
    allowlist: &Allowlist,
) -> Result<(), String> {
    if extra_blocklist.blocks_host(host) {
        return Err(format!("host {host} is blocked by operator blocklist"));
    }
    let resolved = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| format!("failed to resolve host {host}: {e}"))?;
    let allowed = filter_allowed_addrs(host, resolved, extra_blocklist, allowlist);
    if allowed.is_empty() {
        return Err(format!(
            "all resolved IPs for {host} are blocked by SSRF policy"
        ));
    }
    Ok(())
}

impl Resolve for SsrfSafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let extra_blocklist = self.extra_blocklist.clone();
        let allowlist = self.allowlist.clone();
        Box::pin(async move {
            let host = name.as_str().to_string();
            if extra_blocklist.blocks_host(&host) {
                return Err(format!("host {host} is blocked by operator blocklist").into());
            }
            let resolved = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

            let allowed = filter_allowed_addrs(&host, resolved, &extra_blocklist, &allowlist);
            if allowed.is_empty() {
                return Err(
                    format!("all resolved IPs for {host} are blocked by SSRF policy").into(),
                );
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

/// Response statuses worth retrying: rate-limited or transient server-side
/// failure. Anything else (including other 4xx) is the caller's problem, not
/// a transient one, so it's returned as-is on the first attempt.
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// How many HTTP attempts one fetch attempt series ran. Surfaced so the
/// plugin can build `network.request_completed` event payloads
/// (`retry_count = attempts - 1`) without re-counting what `fetch` already
/// tracked internally.
#[derive(Debug, Clone, Copy)]
pub struct FetchStats {
    /// Number of attempts made (1 = no retries).
    pub attempts: u32,
}

/// Terminal outcome of a fetch attempt series: the response (or error) plus
/// the [`FetchStats`] measured along the way.
#[derive(Debug)]
pub struct FetchOutcome {
    pub response: Result<HttpResponseJson, String>,
    pub stats: FetchStats,
    /// First `name=value` pair of every `Set-Cookie` header on the final
    /// attempt's response, when the caller requested cookies
    /// (`use_cookies`); empty otherwise. The plugin (main.rs) merges these
    /// into the caller's jar after the fetch — never logged, never returned
    /// to the caller in the action response.
    pub set_cookies: Vec<(String, String)>,
}

/// Send the HTTP request and map the response, retrying transient failures
/// up to `params.max_retries` times with exponential backoff
/// (`params.retry_backoff_ms`, doubling, capped at
/// [`crate::request::MAX_RETRY_BACKOFF_MS`]). SSRF gating happens inside the
/// `client`'s DNS resolver, not here — see module docs.
///
/// Convenience wrapper over [`fetch_with_stats`] that discards the attempt
/// stats — used by callers that only need the response.
pub async fn fetch(
    client: &reqwest::Client,
    params: &HttpRequestParams,
) -> Result<HttpResponseJson, String> {
    fetch_with_stats(client, params, None, false).await.response
}

/// [`fetch`] plus the [`FetchStats`] the attempt series accumulated, for
/// callers that want to observe how many attempts actually ran.
///
/// Only transient failures are retried. Deterministic ones — a response
/// over [`MAX_BODY_BYTES`], a redirect hop rejected by the client's
/// redirect policy — fail on the first attempt regardless of
/// `max_retries`, since retrying reproduces them exactly.
///
/// `attach_cookies` is a snapshot of the caller's session-cookie jar for
/// the request host (attached only when the caller sent no `Cookie` header
/// itself); `use_cookies` also collects `Set-Cookie` pairs from the
/// response into [`FetchOutcome::set_cookies`].
pub async fn fetch_with_stats(
    client: &reqwest::Client,
    params: &HttpRequestParams,
    attach_cookies: Option<&[(String, String)]>,
    use_cookies: bool,
) -> FetchOutcome {
    let host = reqwest::Url::parse(&params.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let started = std::time::Instant::now();
    let mut backoff_ms = params.retry_backoff_ms;
    let mut attempt = 0;
    let mut set_cookies = Vec::new();

    loop {
        let result = fetch_once(client, params, attach_cookies, use_cookies).await;
        if let Ok((_, cookies)) = &result {
            set_cookies = cookies.clone();
        }
        let retry = attempt < params.max_retries
            && match &result {
                Ok((resp, _)) => is_retryable_status(resp.status),
                Err(e) => e.retryable,
            };

        // One-line JSON per attempt so operators can pipe stdout straight
        // into normal log aggregation instead of parsing a custom format.
        let log_line = serde_json::json!({
            "plugin": "network",
            "method": params.method,
            "host": host,
            "attempt": attempt + 1,
            "status": result.as_ref().ok().map(|(r, _)| r.status),
            "error": result.as_ref().err().map(|e| e.message.as_str()),
            "duration_ms": started.elapsed().as_millis(),
        });
        println!("{log_line}");

        if !retry {
            return FetchOutcome {
                response: result.map(|(r, _)| r).map_err(|e| e.message),
                stats: FetchStats {
                    attempts: attempt + 1,
                },
                set_cookies,
            };
        }
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(crate::request::MAX_RETRY_BACKOFF_MS);
        attempt += 1;
    }
}

/// Method, URL, headers, body/multipart and (opt-in) cookies — everything
/// about the outgoing request except its timeout, which the buffered and
/// streaming paths apply differently.
fn build_request(
    client: &reqwest::Client,
    params: &HttpRequestParams,
    attach_cookies: Option<&[(String, String)]>,
    use_cookies: bool,
) -> Result<reqwest::RequestBuilder, FetchError> {
    let method = reqwest::Method::from_bytes(params.method.as_bytes())
        .map_err(|e| FetchError::deterministic(format!("invalid method: {e}")))?;

    let mut req = client.request(method, &params.url);

    // Multipart bodies override the caller's Content-Type, so build the
    // body first and skip the caller's content-type header below.
    let multipart: Option<(String, Vec<u8>)> = match &params.multipart {
        Some(parts) => {
            if params.body.is_some() || params.body_base64.is_some() {
                return Err(FetchError::deterministic(
                    "multipart is mutually exclusive with body and body_base64".to_string(),
                ));
            }
            let boundary = multipart_boundary();
            let body = build_multipart_body(parts, &boundary).map_err(FetchError::deterministic)?;
            Some((boundary, body))
        }
        None => None,
    };

    for (k, v) in &params.headers {
        if multipart.is_some() && k.eq_ignore_ascii_case("content-type") {
            continue;
        }
        req = req.header(k, v);
    }

    let body_bytes: Option<Vec<u8>> = if let Some((boundary, body)) = multipart {
        req = req.header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        );
        Some(body)
    } else {
        match (&params.body, &params.body_base64) {
            (Some(_), Some(_)) => {
                return Err(FetchError::deterministic(
                    "set body or body_base64, not both".to_string(),
                ))
            }
            (Some(text), None) => Some(text.clone().into_bytes()),
            (None, Some(b64)) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| FetchError::deterministic(format!("invalid body_base64: {e}")))?;
                Some(bytes)
            }
            (None, None) => None,
        }
    };
    if let Some(bytes) = body_bytes {
        req = req.body(bytes);
    }

    // Session cookies (opt-in): the caller's own `Cookie` header wins over
    // the jar; a jar snapshot is attached only when the caller sent none.
    let attach = if use_cookies
        && params
            .headers
            .keys()
            .all(|k| !k.eq_ignore_ascii_case("cookie"))
    {
        attach_cookies.and_then(cookie_header)
    } else {
        None
    };
    if let Some(cookie) = attach {
        req = req.header(reqwest::header::COOKIE, cookie);
    }
    Ok(req)
}

async fn fetch_once(
    client: &reqwest::Client,
    params: &HttpRequestParams,
    attach_cookies: Option<&[(String, String)]>,
    use_cookies: bool,
) -> Result<(HttpResponseJson, Vec<(String, String)>), FetchError> {
    let req = build_request(client, params, attach_cookies, use_cookies)?
        .timeout(Duration::from_millis(params.timeout_ms));

    let mut resp = req.send().await.map_err(|e| {
        if e.is_redirect() {
            // The redirect policy (SSRF-gated hops) rejected a hop — the
            // same chain replays identically on every attempt.
            FetchError::deterministic(format!("request failed: {e}"))
        } else {
            FetchError::transient(format!("request failed: {e}"))
        }
    })?;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();

    // Collect every `Set-Cookie` header on the final response before the
    // header map collapses same-named headers.
    let set_cookies = if use_cookies {
        resp.headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(parse_set_cookie)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let mut body_bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| FetchError::transient(format!("body read error: {e}")))?
    {
        body_bytes.extend_from_slice(&chunk);
        if body_bytes.len() > MAX_BODY_BYTES {
            return Err(FetchError::deterministic(
                "response body exceeds 10 MiB cap".to_string(),
            ));
        }
    }

    let (body, body_encoding) = match String::from_utf8(body_bytes) {
        Ok(text) => (text, "utf8"),
        Err(e) => {
            use base64::Engine;
            (
                base64::engine::general_purpose::STANDARD.encode(e.into_bytes()),
                "base64",
            )
        }
    };

    Ok((
        HttpResponseJson {
            status,
            headers,
            body,
            body_encoding,
            cache: None,
        },
        set_cookies,
    ))
}

/// Generate a `multipart/form-data` boundary: a fixed dash prefix plus hex
/// from the current timestamp and process id. Uniqueness across requests is
/// all that matters here, not unpredictability.
/// CD-03: send the request and hand back the live response for the caller
/// to drain chunk by chunk. `params.timeout_ms` bounds the wait for the
/// response head here; the body side re-applies it per chunk (an idle
/// bound), since a token stream legitimately outlives any whole-request
/// cap. No retries, cache or cookie jar: a partially consumed stream can't
/// be replayed.
pub async fn open_stream(
    client: &reqwest::Client,
    params: &HttpRequestParams,
) -> Result<reqwest::Response, String> {
    let req = build_request(client, params, None, false).map_err(|e| e.message)?;
    match tokio::time::timeout(Duration::from_millis(params.timeout_ms), req.send()).await {
        Err(_) => Err(format!("no response within {} ms", params.timeout_ms)),
        Ok(Err(e)) => Err(format!("request failed: {e}")),
        Ok(Ok(resp)) => Ok(resp),
    }
}

pub fn multipart_boundary() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("------------------------{nanos:x}{:x}", std::process::id())
}

/// Build the `multipart/form-data` wire body for `parts` under `boundary`.
/// Each part becomes a `Content-Disposition: form-data; name=...` block
/// with an optional `filename`, an explicit or defaulted `Content-Type`,
/// then the data. `"` in names/filenames is replaced with `'` and CR/LF are
/// stripped so no part can break out of the framing (no shell is involved —
/// this is pure byte framing).
pub fn build_multipart_body(
    parts: &[crate::request::MultipartPart],
    boundary: &str,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        out.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{}\"",
                sanitize_part_token(&part.name)
            )
            .as_bytes(),
        );
        if let Some(filename) = &part.filename {
            out.extend_from_slice(
                format!("; filename=\"{}\"", sanitize_part_token(filename)).as_bytes(),
            );
        }
        let (default_ct, data) = match (&part.value, &part.file_base64) {
            (Some(v), None) => ("text/plain; charset=utf-8", v.clone().into_bytes()),
            (None, Some(b64)) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| format!("invalid multipart file_base64: {e}"))?;
                ("application/octet-stream", bytes)
            }
            _ => {
                return Err(
                    "each multipart part needs exactly one of value or file_base64".to_string(),
                )
            }
        };
        let content_type = part.content_type.as_deref().unwrap_or(default_ct);
        out.extend_from_slice(format!("\r\nContent-Type: {content_type}\r\n\r\n").as_bytes());
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(out)
}

fn sanitize_part_token(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\r' && *c != '\n')
        .map(|c| if c == '"' { '\'' } else { c })
        .collect()
}

/// Parse the first `name=value` pair of a `Set-Cookie` header, skipping
/// attributes (`Path=...`, `Expires=...`, ...). Control characters are
/// stripped from both name and value so a hostile server cannot inject
/// header framing into the jar. Malformed headers (no `=`) yield `None`.
pub fn parse_set_cookie(header: &str) -> Option<(String, String)> {
    let pair = header.split(';').next()?.trim();
    let (name, value) = pair.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    Some((clean(name), clean(value.trim())))
}

/// Build a `Cookie` request-header value from jar pairs:
/// `name=value; name2=value2`. `None` for an empty jar (no header at all).
pub fn cookie_header(cookies: &[(String, String)]) -> Option<String> {
    if cookies.is_empty() {
        return None;
    }
    Some(
        cookies
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// Hard ceiling on cache entries per plugin process.
pub const CACHE_MAX_ENTRIES: usize = 128;
/// Hard ceiling on the total cached body bytes (~8 MiB). A single body
/// larger than this never enters the cache.
pub const CACHE_MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// One cached response, keyed per caller+method+url+body-hash.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub stored_at_ms: u64,
    pub expires_at_ms: u64,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub body_encoding: &'static str,
}

/// Bounded in-memory response cache. Per-caller by construction (the key
/// embeds `caller_plugin_id`), so no caller can see another caller's cached
/// data. Evicts the oldest entry (by `stored_at_ms`) when over
/// [`CACHE_MAX_ENTRIES`] or [`CACHE_MAX_TOTAL_BYTES`].
#[derive(Default)]
pub struct CacheStore {
    entries: HashMap<String, CacheEntry>,
    total_bytes: usize,
}

impl CacheStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached entry for `key` when present and not yet expired.
    pub fn get(&self, key: &str, now_ms: u64) -> Option<&CacheEntry> {
        self.entries.get(key).filter(|e| now_ms <= e.expires_at_ms)
    }

    /// Insert or replace `key`, then evict the oldest entries until the
    /// size bounds hold. A body larger than the whole budget never enters.
    pub fn put(&mut self, key: String, entry: CacheEntry) {
        if entry.body.len() > CACHE_MAX_TOTAL_BYTES {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(old.body.len());
        }
        self.total_bytes = self.total_bytes.saturating_add(entry.body.len());
        self.entries.insert(key, entry);
        while self.entries.len() > CACHE_MAX_ENTRIES || self.total_bytes > CACHE_MAX_TOTAL_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.stored_at_ms)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.body.len());
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Cache key for one request: per-caller so cached data never crosses
/// callers.
pub fn cache_key(caller_plugin_id: &str, method: &str, url: &str, body_hash: u64) -> String {
    format!("{caller_plugin_id}|{method}|{url}|{body_hash:x}")
}

/// Deterministic body fingerprint for the cache key. Multipart requests
/// hash the canonical part content, not the wire bytes — the boundary is
/// random per request, so identical multipart uploads must share a key.
pub fn request_body_hash(params: &HttpRequestParams) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match &params.multipart {
        Some(parts) => {
            "multipart".hash(&mut hasher);
            for part in parts {
                part.name.hash(&mut hasher);
                part.value.hash(&mut hasher);
                part.file_base64.hash(&mut hasher);
                part.filename.hash(&mut hasher);
                part.content_type.hash(&mut hasher);
            }
        }
        None => {
            params.body.hash(&mut hasher);
            params.body_base64.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Whether a response may be cached: a 2xx that did not opt out via
/// `Cache-Control: no-store`.
pub fn is_cacheable(status: u16, headers: &HashMap<String, String>) -> bool {
    (200..300).contains(&status)
        && headers.iter().all(|(k, v)| {
            !(k.eq_ignore_ascii_case("cache-control")
                && v.to_ascii_lowercase().contains("no-store"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn params(url: String) -> HttpRequestParams {
        HttpRequestParams {
            method: "GET".into(),
            url,
            headers: HashMap::new(),
            body: None,
            body_base64: None,
            multipart: None,
            cache_ttl_ms: 0,
            use_cookies: false,
            timeout_ms: 5000,
            max_retries: 0,
            retry_backoff_ms: 1,
            follow_redirects: false,
            max_redirects: crate::request::MAX_REDIRECTS,
        }
    }

    async fn mock_server(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}/")
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[tokio::test]
    async fn fetch_returns_status_headers_body() {
        let url = mock_server(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 5\r\n\r\nhello",
        )
        .await;
        let client = reqwest::Client::new();
        let resp = fetch(&client, &params(url)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.headers.get("content-type").unwrap(), "text/plain");
        assert_eq!(resp.body, "hello");
    }

    #[tokio::test]
    async fn fetch_base64_encodes_non_utf8_body() {
        let raw_body: &[u8] = &[0xff, 0xfe, 0xfd, 0x00, 0x01];
        let mut response = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\n".to_vec();
        response.extend_from_slice(raw_body);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&response).await;
        });
        let client = reqwest::Client::new();
        let resp = fetch(&client, &params(format!("http://{addr}/")))
            .await
            .unwrap();
        assert_eq!(resp.body_encoding, "base64");
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&resp.body)
            .unwrap();
        assert_eq!(decoded, raw_body);
    }

    #[tokio::test]
    async fn fetch_returns_utf8_encoding_for_text_body() {
        let url = mock_server("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello").await;
        let client = reqwest::Client::new();
        let resp = fetch(&client, &params(url)).await.unwrap();
        assert_eq!(resp.body_encoding, "utf8");
    }

    #[tokio::test]
    async fn fetch_sends_body_base64_byte_exact() {
        let raw_body: &[u8] = &[0x00, 0x01, 0x02, 0xff, 0xfe, 0x7f];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Read the full request: headers (up to \r\n\r\n) then exactly
            // content-length body bytes, and echo the body back raw.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            let header_end;
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let content_length = headers
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while buf.len() < header_end + content_length {
                let n = socket.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = &buf[header_end..header_end + content_length];
            let response = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
            let mut out = response.into_bytes();
            out.extend_from_slice(body);
            let _ = socket.write_all(&out).await;
        });
        let client = reqwest::Client::new();
        use base64::Engine;
        let mut p = params(format!("http://{addr}/"));
        p.method = "POST".into();
        p.body_base64 = Some(base64::engine::general_purpose::STANDARD.encode(raw_body));
        let resp = fetch(&client, &p).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_encoding, "base64");
        let echoed = base64::engine::general_purpose::STANDARD
            .decode(&resp.body)
            .unwrap();
        assert_eq!(echoed, raw_body);
    }

    #[tokio::test]
    async fn fetch_rejects_body_and_body_base64_together() {
        let url = mock_server("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
        let client = reqwest::Client::new();
        let mut p = params(url);
        p.body = Some("text".to_string());
        p.body_base64 = Some("AAEC/w==".to_string());
        let err = fetch(&client, &p).await.unwrap_err();
        assert!(err.contains("not both"), "error was: {err}");
    }

    #[tokio::test]
    async fn fetch_errors_on_body_over_cap() {
        let big_body = "x".repeat(MAX_BODY_BYTES + 1);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
            big_body.len(),
            big_body
        );
        let url = mock_server(Box::leak(response.into_boxed_str())).await;
        let client = reqwest::Client::new();
        let err = fetch(&client, &params(url)).await.unwrap_err();
        assert!(err.contains("10 MiB"), "error was: {err}");
    }

    #[tokio::test]
    async fn fetch_does_not_follow_redirect_by_default() {
        let url = mock_server(
            "HTTP/1.1 302 Found\r\nlocation: http://example.invalid/\r\ncontent-length: 0\r\n\r\n",
        )
        .await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resp = fetch(&client, &params(url)).await.unwrap();
        assert_eq!(resp.status, 302);
    }

    #[tokio::test]
    async fn fetch_follows_redirect_when_client_allows_it() {
        let final_addr = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                    .await;
            });
            addr
        };
        let redirect_response = format!(
            "HTTP/1.1 302 Found\r\nlocation: http://{final_addr}/\r\ncontent-length: 0\r\n\r\n"
        );
        let url = mock_server(Box::leak(redirect_response.into_boxed_str())).await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .unwrap();
        let resp = fetch(&client, &params(url)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "ok");
    }

    #[tokio::test]
    async fn fetch_retries_on_retryable_status_then_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response in [
                "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n",
                "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let client = reqwest::Client::new();
        let mut p = params(format!("http://{addr}/"));
        p.max_retries = 1;
        let resp = fetch(&client, &p).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "ok");
    }

    #[tokio::test]
    async fn fetch_does_not_retry_non_retryable_status() {
        let url = mock_server("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n").await;
        let client = reqwest::Client::new();
        let mut p = params(url);
        p.max_retries = 3;
        let resp = fetch(&client, &p).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn fetch_errors_on_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            // Never write a response — force the client-side timeout.
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(socket);
        });
        let client = reqwest::Client::new();
        let mut p = params(format!("http://{addr}/"));
        p.timeout_ms = 100;
        let err = fetch(&client, &p).await.unwrap_err();
        assert!(err.contains("request failed"), "error was: {err}");
    }

    /// Spawn a server that serves `response` to every connection it
    /// accepts, forever, counting how many connections were made. Returns
    /// the URL and the shared connection counter.
    async fn counting_server(response: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let connections_clone = connections.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                connections_clone.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}/"), connections)
    }

    #[tokio::test]
    async fn fetch_does_not_retry_body_over_cap() {
        let big_body = "x".repeat(MAX_BODY_BYTES + 1);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
            big_body.len(),
            big_body
        );
        let (url, connections) = counting_server(Box::leak(response.into_boxed_str())).await;
        let client = reqwest::Client::new();
        let mut p = params(url);
        p.max_retries = 3;
        let err = fetch(&client, &p).await.unwrap_err();
        assert!(err.contains("10 MiB"), "error was: {err}");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "body-over-cap is deterministic and must not be retried"
        );
    }

    #[tokio::test]
    async fn fetch_does_not_retry_redirect_policy_rejection() {
        let (url, connections) = counting_server(
            "HTTP/1.1 302 Found\r\nlocation: http://example.invalid/\r\ncontent-length: 0\r\n\r\n",
        )
        .await;
        // Rejects every redirect — same shape as the SSRF-gated policy in
        // main.rs when a hop lands on a blocked host.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                attempt.error("redirect rejected by policy")
            }))
            .build()
            .unwrap();
        let mut p = params(url);
        p.max_retries = 3;
        let err = fetch(&client, &p).await.unwrap_err();
        assert!(err.contains("error following redirect"), "error was: {err}");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "a rejected redirect hop is deterministic and must not be retried"
        );
    }

    #[tokio::test]
    async fn check_host_reachable_blocks_loopback() {
        let err = check_host_reachable("localhost", &Blocklist::default(), &Allowlist::default())
            .await
            .unwrap_err();
        assert!(err.contains("blocked"), "error was: {err}");
    }

    #[tokio::test]
    async fn check_host_reachable_allowlist_permits_loopback() {
        let allowlist = Allowlist::parse("localhost");
        assert!(
            check_host_reachable("localhost", &Blocklist::default(), &allowlist)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn check_host_reachable_honors_extra_blocklist() {
        let blocklist = Blocklist::parse("localhost");
        let err = check_host_reachable("localhost", &blocklist, &Allowlist::default())
            .await
            .unwrap_err();
        assert!(err.contains("operator blocklist"), "error was: {err}");
    }

    #[test]
    fn parse_set_cookie_extracts_first_pair() {
        assert_eq!(
            parse_set_cookie("session=abc123; Path=/; HttpOnly"),
            Some(("session".to_string(), "abc123".to_string()))
        );
        assert_eq!(
            parse_set_cookie("a=b=c"),
            Some(("a".to_string(), "b=c".to_string()))
        );
        assert_eq!(
            parse_set_cookie("  padded = value "),
            Some(("padded".to_string(), "value".to_string()))
        );
    }

    #[test]
    fn parse_set_cookie_rejects_malformed_and_control_chars() {
        assert_eq!(parse_set_cookie("no-equals-here"), None);
        assert_eq!(parse_set_cookie(""), None);
        assert_eq!(parse_set_cookie("=val"), None);
        let header = "evil=va\u{1b}lue; Path=/";
        let (name, value) = parse_set_cookie(header).unwrap();
        assert_eq!(name, "evil");
        assert_eq!(value, "value", "control chars stripped");
    }

    #[test]
    fn cookie_header_joins_pairs_and_none_for_empty() {
        assert_eq!(cookie_header(&[]), None);
        assert_eq!(
            cookie_header(&[
                ("session".to_string(), "abc".to_string()),
                ("theme".to_string(), "dark".to_string()),
            ]),
            Some("session=abc; theme=dark".to_string())
        );
    }

    #[test]
    fn multipart_body_frames_parts_and_terminates() {
        use crate::request::MultipartPart;
        let parts = vec![
            MultipartPart {
                name: "note".into(),
                value: Some("hello".into()),
                file_base64: None,
                filename: None,
                content_type: None,
            },
            MultipartPart {
                name: "file".into(),
                value: None,
                file_base64: Some("aGVsbG8=".into()), // "hello" — keeps the body UTF-8
                filename: Some("blob.bin".into()),
                content_type: Some("application/octet-stream".into()),
            },
        ];
        let boundary = "----testboundary";
        let body = build_multipart_body(&parts, boundary).unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(
            text.starts_with("------testboundary\r\n"),
            "wire body opens with --{boundary}: {text}"
        );
        assert!(text.contains("Content-Disposition: form-data; name=\"note\""));
        assert!(text.contains("Content-Type: text/plain; charset=utf-8\r\n\r\nhello\r\n"));
        assert!(text.contains("; filename=\"blob.bin\""));
        assert!(text.contains("Content-Type: application/octet-stream\r\n\r\nhello\r\n"));
        assert!(text.ends_with("----testboundary--\r\n"));
    }

    #[test]
    fn multipart_body_sanitizes_quotes_and_crlf_in_tokens() {
        use crate::request::MultipartPart;
        let parts = vec![MultipartPart {
            name: "a\"b\r\nc".into(),
            value: Some("v".into()),
            file_base64: None,
            filename: Some("f\"n\r\n".into()),
            content_type: None,
        }];
        let body = build_multipart_body(&parts, "----b").unwrap();
        let text = String::from_utf8(body).unwrap();
        // CR/LF are stripped, `"` -> `'`: `a"b\r\nc` -> `a'bc`, `f"n\r\n` -> `f'n`.
        assert!(
            text.contains("name=\"a'bc\""),
            "quote escaped, CR/LF stripped: {text}"
        );
        assert!(
            text.contains("filename=\"f'n\""),
            "quote escaped, CR/LF stripped: {text}"
        );
        assert!(!text.contains("a\"b"), "raw quote gone");
        assert!(!text.contains("f\"n"), "raw quote gone");
    }

    #[test]
    fn cache_store_hit_and_expiry() {
        let mut store = CacheStore::new();
        let entry = CacheEntry {
            stored_at_ms: 0,
            expires_at_ms: 100,
            status: 200,
            headers: HashMap::new(),
            body: b"hello".to_vec(),
            body_encoding: "utf8",
        };
        store.put("a|GET|u|0".to_string(), entry);
        assert_eq!(store.len(), 1);
        assert!(store.get("a|GET|u|0", 99).is_some(), "fresh");
        assert!(
            store.get("a|GET|u|0", 100).is_some(),
            "expiry is <= (inclusive)"
        );
        assert!(store.get("a|GET|u|0", 101).is_none(), "expired");
        assert!(store.get("b|GET|u|0", 0).is_none(), "different key");
    }

    #[test]
    fn cache_store_evicts_oldest_over_limits() {
        let mut store = CacheStore::new();
        for i in 0..CACHE_MAX_ENTRIES + 5 {
            store.put(
                format!("k{i}"),
                CacheEntry {
                    stored_at_ms: i as u64,
                    expires_at_ms: u64::MAX,
                    status: 200,
                    headers: HashMap::new(),
                    body: vec![1u8; 16],
                    body_encoding: "utf8",
                },
            );
        }
        assert_eq!(store.len(), CACHE_MAX_ENTRIES);
        assert!(store.get("k0", 0).is_none(), "oldest evicted");
        assert!(
            store
                .get(&format!("k{}", CACHE_MAX_ENTRIES + 4), 0)
                .is_some(),
            "newest kept"
        );
    }

    #[test]
    fn cache_store_rejects_body_over_total_budget() {
        let mut store = CacheStore::new();
        store.put(
            "big".to_string(),
            CacheEntry {
                stored_at_ms: 0,
                expires_at_ms: u64::MAX,
                status: 200,
                headers: HashMap::new(),
                body: vec![0u8; CACHE_MAX_TOTAL_BYTES + 1],
                body_encoding: "utf8",
            },
        );
        assert!(store.is_empty(), "oversized body never enters");
    }

    #[test]
    fn cache_key_is_per_caller() {
        let k1 = cache_key("caller_a", "GET", "https://x/", 7);
        let k2 = cache_key("caller_b", "GET", "https://x/", 7);
        assert_ne!(k1, k2);
        let k3 = cache_key("caller_a", "POST", "https://x/", 7);
        assert_ne!(k1, k3);
    }

    #[test]
    fn request_body_hash_distinguishes_bodies_but_not_multipart_boundaries() {
        let mut p1 = params("https://x/".into());
        p1.body = Some("alpha".into());
        let mut p2 = params("https://x/".into());
        p2.body = Some("beta".into());
        assert_ne!(request_body_hash(&p1), request_body_hash(&p2));

        use crate::request::MultipartPart;
        let part = MultipartPart {
            name: "a".into(),
            value: Some("v".into()),
            file_base64: None,
            filename: None,
            content_type: None,
        };
        let mut m1 = params("https://x/".into());
        m1.multipart = Some(vec![part.clone()]);
        let mut m2 = params("https://x/".into());
        m2.multipart = Some(vec![part.clone()]);
        assert_eq!(
            request_body_hash(&m1),
            request_body_hash(&m2),
            "same content, same hash"
        );
        let mut m3 = params("https://x/".into());
        m3.body = Some("not multipart".into());
        assert_ne!(request_body_hash(&m1), request_body_hash(&m3));
    }

    #[test]
    fn is_cacheable_only_for_2xx_without_no_store() {
        let mut h = HashMap::new();
        assert!(is_cacheable(200, &h));
        assert!(!is_cacheable(404, &h), "non-2xx not cacheable");
        h.insert("Cache-Control".into(), "max-age=60".into());
        assert!(is_cacheable(200, &h), "no-store absent");
        h.insert("cache-control".into(), "no-cache, no-store".into());
        assert!(!is_cacheable(200, &h), "no-store present");
    }
}
