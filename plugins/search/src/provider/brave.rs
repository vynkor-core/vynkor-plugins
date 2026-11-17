//! Brave Search API adapter.
//!
//! `GET {base_url}/res/v1/web/search?q=<query>&count=<count>` with header
//! `X-Subscription-Token: <key>`. Response `web.results[]` →
//! `title`/`url`/`description` (→ `snippet`).
//!
//! Wire shape verified against Brave's Search API docs (2026-08):
//!   https://api-dashboard.search.brave.com/app/documentation/web-search
//!   https://api-dashboard.search.brave.com/api-reference/web/search/get
//! `count` is min 1 / max 20 (default 20); `web` is nullable (absent when no
//! web results), each result carries `title`, `url`, `description`.

use std::collections::HashMap;

use super::{HttpRequestJson, Provider, SearchResult};
use crate::request::WebSearchParams;

pub struct BraveProvider;

impl Provider for BraveProvider {
    fn build_http_request(&self, params: &WebSearchParams, api_key: &str) -> HttpRequestJson {
        let base = params.base_url.trim_end_matches('/');
        let url = format!(
            "{base}/res/v1/web/search?q={}&count={}",
            urlencode_query(&params.query),
            params.count
        );

        let mut headers = HashMap::new();
        headers.insert("X-Subscription-Token".to_string(), api_key.to_string());
        headers.insert("Accept".to_string(), "application/json".to_string());

        HttpRequestJson {
            method: "GET",
            url,
            headers,
            body: String::new(),
            timeout_ms: params.timeout_ms,
        }
    }

    fn parse_response(&self, body: &[u8]) -> Result<Vec<SearchResult>, String> {
        #[derive(serde::Deserialize)]
        struct WebResult {
            #[serde(default)]
            title: String,
            #[serde(default)]
            url: String,
            #[serde(default)]
            description: String,
        }
        #[derive(serde::Deserialize)]
        struct Web {
            #[serde(default)]
            results: Vec<WebResult>,
        }
        #[derive(serde::Deserialize)]
        struct Response {
            #[serde(default)]
            web: Option<Web>,
        }

        let resp: Response =
            serde_json::from_slice(body).map_err(|e| format!("malformed brave response: {e}"))?;

        // `web` is nullable: a query returning only non-web verticals (news,
        // videos, ...) carries `web: null` — an empty list, not an error.
        Ok(resp
            .web
            .map(|w| {
                w.results
                    .into_iter()
                    .map(|r| SearchResult {
                        title: r.title,
                        url: r.url,
                        snippet: r.description,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Percent-encode `s` for use in a URL query string: every byte outside the
/// RFC 3986 unreserved set becomes `%XX` (UTF-8 bytes, space → `%20`).
fn urlencode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Provider as ReqProvider;

    fn params() -> WebSearchParams {
        WebSearchParams {
            query: "vynkor plugins".to_string(),
            provider: ReqProvider::Brave,
            api_key_env: "SEARCH_BRAVE_KEY".to_string(),
            base_url: "https://api.search.brave.com".to_string(),
            count: 5,
            timeout_ms: 30_000,
        }
    }

    #[test]
    fn builds_get_request_with_token_header() {
        let req = BraveProvider.build_http_request(&params(), "vault-key-123");
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.url,
            "https://api.search.brave.com/res/v1/web/search?q=vynkor%20plugins&count=5"
        );
        assert_eq!(
            req.headers.get("X-Subscription-Token").map(String::as_str),
            Some("vault-key-123")
        );
        assert!(req.body.is_empty());
    }

    #[test]
    fn key_never_appears_in_url_or_body() {
        let req = BraveProvider.build_http_request(&params(), "vault-key-123");
        assert!(!req.url.contains("vault-key-123"));
        assert!(!req.body.contains("vault-key-123"));
    }

    #[test]
    fn strips_trailing_slash_from_base_url() {
        let mut p = params();
        p.base_url = "https://api.search.brave.com/".to_string();
        let req = BraveProvider.build_http_request(&p, "k");
        assert_eq!(
            req.url,
            "https://api.search.brave.com/res/v1/web/search?q=vynkor%20plugins&count=5"
        );
    }

    #[test]
    fn parses_valid_response() {
        let body = serde_json::json!({
            "web": {
                "results": [
                    {
                        "title": "vynkor",
                        "url": "https://example.com/vynkor",
                        "description": "A plugin kernel"
                    }
                ]
            }
        })
        .to_string();
        let results = BraveProvider.parse_response(body.as_bytes()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "vynkor");
        assert_eq!(results[0].url, "https://example.com/vynkor");
        assert_eq!(results[0].snippet, "A plugin kernel");
    }

    #[test]
    fn rejects_malformed_json() {
        let err = BraveProvider.parse_response(b"not json").unwrap_err();
        assert!(err.contains("malformed brave response"), "error was: {err}");
    }

    #[test]
    fn missing_fields_default_to_empty() {
        let body = serde_json::json!({
            "web": { "results": [ { "url": "https://example.com/x" } ] }
        })
        .to_string();
        let results = BraveProvider.parse_response(body.as_bytes()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "");
        assert_eq!(results[0].snippet, "");
    }

    #[test]
    fn null_web_block_yields_empty_results() {
        let body = serde_json::json!({ "web": null }).to_string();
        let results = BraveProvider.parse_response(body.as_bytes()).unwrap();
        assert!(results.is_empty());
    }
}
