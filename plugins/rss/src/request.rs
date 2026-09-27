use serde::Deserialize;
#[derive(Debug)]
pub enum RssRequest {
    Add { url: String },
    Remove { id: String },
    List,
    Fetch { id: String, timeout_ms: u64 },
    FetchAll { timeout_ms: u64 },
    Articles { feed_id: Option<String>, unread_only: bool, query: Option<String>, limit: usize, offset: usize },
    MarkRead { id: String, read: bool },
    Status,
}
#[derive(Deserialize)] struct AddParams { url: String }
#[derive(Deserialize)] struct IdParams { id: String }
#[derive(Deserialize)] struct FetchParams { id: String, timeout_ms: Option<u64> }
#[derive(Deserialize)] struct FetchAllParams { timeout_ms: Option<u64> }
#[derive(Deserialize)] struct ArticlesParams { feed_id: Option<String>, unread_only: Option<bool>, query: Option<String>, limit: Option<usize>, offset: Option<usize> }
#[derive(Deserialize)] struct MarkReadParams { id: String, read: Option<bool> }

fn validate_url(url: &str) -> Result<String, String> {
    let u = url.trim().to_string();
    if u.is_empty() { return Err("params.url must not be empty".into()); }
    if !u.starts_with("http://") && !u.starts_with("https://") { return Err("params.url must start with http:// or https://".into()); }
    if u.len() > 2048 { return Err("params.url exceeds 2048 bytes".into()); }
    Ok(u)
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<RssRequest, String> {
    match action {
        "rss_add" => {
            let p: AddParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for rss_add: {e}"))?;
            Ok(RssRequest::Add { url: validate_url(&p.url)? })
        }
        "rss_remove" => {
            let p: IdParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for rss_remove: {e}"))?;
            if p.id.trim().is_empty() { return Err("params.id must not be empty".into()); }
            Ok(RssRequest::Remove { id: p.id.trim().to_string() })
        }
        "rss_list" => Ok(RssRequest::List),
        "rss_fetch" => {
            let p: FetchParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for rss_fetch: {e}"))?;
            if p.id.trim().is_empty() { return Err("params.id must not be empty".into()); }
            let timeout_ms = p.timeout_ms.unwrap_or(10000);
            if timeout_ms==0 || timeout_ms>30000 { return Err("params.timeout_ms must be 1..30000".into()); }
            Ok(RssRequest::Fetch { id: p.id.trim().to_string(), timeout_ms })
        }
        "rss_fetch_all" => {
            let p: FetchAllParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for rss_fetch_all: {e}"))?;
            let timeout_ms = p.timeout_ms.unwrap_or(10000);
            if timeout_ms==0 || timeout_ms>30000 { return Err("params.timeout_ms must be 1..30000".into()); }
            Ok(RssRequest::FetchAll { timeout_ms })
        }
        "rss_articles" => {
            let p: ArticlesParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for rss_articles: {e}"))?;
            let limit = p.limit.unwrap_or(50);
            if limit==0 || limit>500 { return Err("params.limit must be 1..500".into()); }
            Ok(RssRequest::Articles { feed_id: p.feed_id.and_then(|s| { let t=s.trim().to_string(); if t.is_empty() {None} else {Some(t)} }), unread_only: p.unread_only.unwrap_or(false), query: p.query.and_then(|s| { let t=s.trim().to_string(); if t.is_empty() {None} else {Some(t)} }), limit, offset: p.offset.unwrap_or(0) })
        }
        "rss_mark_read" => {
            let p: MarkReadParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for rss_mark_read: {e}"))?;
            if p.id.trim().is_empty() { return Err("params.id must not be empty".into()); }
            Ok(RssRequest::MarkRead { id: p.id.trim().to_string(), read: p.read.unwrap_or(true) })
        }
        "rss_status" => Ok(RssRequest::Status),
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn add_ok() { assert!(parse_request("rss_add", br#"{"url":"https://example.com/feed.xml"}"#).is_ok()); }
    #[test]
    fn add_rejects_bad() { assert!(parse_request("rss_add", br#"{"url":"ftp://x"}"#).is_err()); }
    #[test]
    fn articles_defaults() { match parse_request("rss_articles", b"{}").unwrap() { RssRequest::Articles { limit, .. } => assert_eq!(limit,50), _=>panic!() } }
}
