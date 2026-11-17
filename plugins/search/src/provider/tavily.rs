//! Tavily Search API adapter.
//!
//! `POST {base_url}/search` with JSON body `{"query": ..., "max_results": ...}`
//! and header `Authorization: Bearer <key>`. Response `results[]` →
//! `title`/`url`/`content` (→ `snippet`).
//!
//! Wire shape verified against Tavily's API docs (2026-08):
//!   https://docs.tavily.com/documentation/api-reference/endpoint/search
//! `max_results` is default 5 / max 20; each result carries `title`, `url`,
//! `content` (plus `score`/`raw_content` we ignore).

use std::collections::HashMap;

use super::{HttpRequestJson, Provider, SearchResult};
use crate::request::WebSearchParams;

pub struct TavilyProvider;

impl Provider for TavilyProvider {
    fn build_http_request(&self, params: &WebSearchParams, api_key: &str) -> HttpRequestJson {
        let url = format!("{}/search", params.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "query": params.query,
            "max_results": params.count,
        })
        .to_string();

        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        headers.insert("content-type".to_string(), "application/json".to_string());

        HttpRequestJson {
            method: "POST",
            url,
            headers,
            body,
            timeout_ms: params.timeout_ms,
        }
    }

    fn parse_response(&self, body: &[u8]) -> Result<Vec<SearchResult>, String> {
        #[derive(serde::Deserialize)]
        struct TavilyResult {
            #[serde(default)]
            title: String,
            #[serde(default)]
            url: String,
            #[serde(default)]
            content: String,
        }
        #[derive(serde::Deserialize)]
        struct Response {
            #[serde(default)]
            results: Vec<TavilyResult>,
        }

        let resp: Response =
            serde_json::from_slice(body).map_err(|e| format!("malformed tavily response: {e}"))?;

        Ok(resp
            .results
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.content,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Provider as ReqProvider;

    fn params() -> WebSearchParams {
        WebSearchParams {
            query: "vynkor plugins".to_string(),
            provider: ReqProvider::Tavily,
            api_key_env: "SEARCH_TAVILY_KEY".to_string(),
            base_url: "https://api.tavily.com".to_string(),
            count: 5,
            timeout_ms: 30_000,
        }
    }

    #[test]
    fn builds_post_request_with_bearer_auth() {
        let req = TavilyProvider.build_http_request(&params(), "vault-key-123");
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://api.tavily.com/search");
        assert_eq!(
            req.headers.get("Authorization").map(String::as_str),
            Some("Bearer vault-key-123")
        );
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["query"], "vynkor plugins");
        assert_eq!(body["max_results"], 5);
    }

    #[test]
    fn key_never_appears_in_url_or_body() {
        let req = TavilyProvider.build_http_request(&params(), "vault-key-123");
        assert!(!req.url.contains("vault-key-123"));
        assert!(!req.body.contains("vault-key-123"));
    }

    #[test]
    fn strips_trailing_slash_from_base_url() {
        let mut p = params();
        p.base_url = "https://api.tavily.com/".to_string();
        let req = TavilyProvider.build_http_request(&p, "k");
        assert_eq!(req.url, "https://api.tavily.com/search");
    }

    #[test]
    fn parses_valid_response() {
        let body = serde_json::json!({
            "results": [
                {
                    "title": "vynkor",
                    "url": "https://example.com/vynkor",
                    "content": "A plugin kernel"
                }
            ]
        })
        .to_string();
        let results = TavilyProvider.parse_response(body.as_bytes()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "vynkor");
        assert_eq!(results[0].url, "https://example.com/vynkor");
        assert_eq!(results[0].snippet, "A plugin kernel");
    }

    #[test]
    fn rejects_malformed_json() {
        let err = TavilyProvider.parse_response(b"not json").unwrap_err();
        assert!(
            err.contains("malformed tavily response"),
            "error was: {err}"
        );
    }

    #[test]
    fn missing_fields_default_to_empty() {
        let body = serde_json::json!({
            "results": [ { "title": "only a title" } ]
        })
        .to_string();
        let results = TavilyProvider.parse_response(body.as_bytes()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "only a title");
        assert_eq!(results[0].url, "");
        assert_eq!(results[0].snippet, "");
    }

    #[test]
    fn empty_results_is_a_valid_outcome() {
        let body = serde_json::json!({ "results": [] }).to_string();
        let results = TavilyProvider.parse_response(body.as_bytes()).unwrap();
        assert!(results.is_empty());
    }
}
