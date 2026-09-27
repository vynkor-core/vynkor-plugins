use serde::Deserialize;

#[derive(Debug)]
pub enum MetricsRequest {
    Query { from_ms: Option<i64>, to_ms: Option<i64>, limit: usize, offset: usize },
    Latest,
    Stats,
    Status,
}

#[derive(Deserialize)]
struct QueryParams {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<MetricsRequest, String> {
    match action {
        "metrics_query" => {
            let p: QueryParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for metrics_query: {e}"))?;
            let limit = p.limit.unwrap_or(100);
            if limit == 0 || limit > 500 { return Err("params.limit must be 1..500".into()); }
            Ok(MetricsRequest::Query { from_ms: p.from_ms, to_ms: p.to_ms, limit, offset: p.offset.unwrap_or(0) })
        }
        "metrics_latest" => Ok(MetricsRequest::Latest),
        "metrics_stats" => Ok(MetricsRequest::Stats),
        "metrics_status" => Ok(MetricsRequest::Status),
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_defaults() {
        let r = parse_request("metrics_query", b"{}").unwrap();
        match r { MetricsRequest::Query { limit, .. } => assert_eq!(limit, 100), _=>panic!() }
    }
    #[test]
    fn unknown() { assert!(parse_request("bad", b"{}").is_err()); }
}
