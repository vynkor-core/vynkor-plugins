use serde::Deserialize;
#[derive(Debug)]
pub enum UptimeRequest {
    Add { url: String },
    Remove { id: String },
    List,
    Check { url: String, timeout_ms: u64 },
    History { url: Option<String>, limit: usize, offset: usize },
    Status,
}
#[derive(Deserialize)] struct AddParams { url: String }
#[derive(Deserialize)] struct IdParams { id: String }
#[derive(Deserialize)] struct CheckParams { url: String, timeout_ms: Option<u64> }
#[derive(Deserialize)] struct HistoryParams { url: Option<String>, limit: Option<usize>, offset: Option<usize> }

fn validate_url(url: &str) -> Result<String, String> {
    let u = url.trim().to_string();
    if u.is_empty() { return Err("params.url must not be empty".into()); }
    if !u.starts_with("http://") && !u.starts_with("https://") { return Err("params.url must start with http:// or https://".into()); }
    if u.len() > 2048 { return Err("params.url exceeds 2048 bytes".into()); }
    Ok(u)
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<UptimeRequest, String> {
    match action {
        "uptime_add" => {
            let p: AddParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for uptime_add: {e}"))?;
            Ok(UptimeRequest::Add { url: validate_url(&p.url)? })
        }
        "uptime_remove" => {
            let p: IdParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for uptime_remove: {e}"))?;
            if p.id.trim().is_empty() { return Err("params.id must not be empty".into()); }
            Ok(UptimeRequest::Remove { id: p.id.trim().to_string() })
        }
        "uptime_list" => Ok(UptimeRequest::List),
        "uptime_check" => {
            let p: CheckParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for uptime_check: {e}"))?;
            let url = validate_url(&p.url)?;
            let timeout_ms = p.timeout_ms.unwrap_or(5000);
            if timeout_ms==0 || timeout_ms>30000 { return Err("params.timeout_ms must be 1..30000".into()); }
            Ok(UptimeRequest::Check { url, timeout_ms })
        }
        "uptime_history" => {
            let p: HistoryParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for uptime_history: {e}"))?;
            let limit = p.limit.unwrap_or(100);
            if limit==0 || limit>500 { return Err("params.limit must be 1..500".into()); }
            let url = p.url.and_then(|u| { let t=u.trim().to_string(); if t.is_empty() {None} else {Some(t)} });
            Ok(UptimeRequest::History { url, limit, offset: p.offset.unwrap_or(0) })
        }
        "uptime_status" => Ok(UptimeRequest::Status),
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn add_ok() { let r=parse_request("uptime_add", br#"{"url":"https://example.com"}"#).unwrap(); match r { UptimeRequest::Add { url } => assert_eq!(url, "https://example.com"), _=>panic!() } }
    #[test]
    fn add_rejects_bad_url() { assert!(parse_request("uptime_add", br#"{"url":"ftp://x"}"#).is_err()); }
    #[test]
    fn check_defaults() { let r=parse_request("uptime_check", br#"{"url":"https://example.com"}"#).unwrap(); match r { UptimeRequest::Check { timeout_ms, .. } => assert_eq!(timeout_ms, 5000), _=>panic!() } }
}
