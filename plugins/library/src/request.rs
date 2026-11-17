use serde::Deserialize;

#[derive(Debug)]
pub enum LibraryRequest {
    Scan {
        roots: Option<Vec<String>>,
        force: bool,
    },
    Search {
        query: Option<String>,
        kind: String,
        limit: usize,
        offset: usize,
    },
    Get {
        id: String,
    },
    Random {
        kind: String,
        count: usize,
    },
    Recent {
        kind: String,
        limit: usize,
    },
    Stats,
}

#[derive(Deserialize)]
struct ScanParams {
    #[serde(default)]
    roots: Option<Vec<String>>,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
struct SearchParams {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Deserialize)]
struct GetParams {
    id: String,
}

#[derive(Deserialize)]
struct RandomParams {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    count: Option<usize>,
}

#[derive(Deserialize)]
struct RecentParams {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn normalize_kind(s: Option<String>) -> String {
    match s.as_deref().map(|v| v.trim().to_ascii_lowercase()) {
        Some(k) if k == "audio" || k == "photo" || k == "video" => k,
        _ => "all".to_string(),
    }
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<LibraryRequest, String> {
    match action {
        "library_scan" => {
            let p: ScanParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for library_scan: {e}"))?;
            Ok(LibraryRequest::Scan {
                roots: p.roots,
                force: p.force,
            })
        }
        "library_search" => {
            let p: SearchParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for library_search: {e}"))?;
            let limit = p.limit.unwrap_or(20);
            if limit == 0 || limit > 200 {
                return Err("params.limit must be 1..200".into());
            }
            Ok(LibraryRequest::Search {
                query: p.query.and_then(|q| {
                    let t = q.trim().to_string();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                }),
                kind: normalize_kind(p.kind),
                limit,
                offset: p.offset.unwrap_or(0),
            })
        }
        "library_get" => {
            let p: GetParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for library_get: {e}"))?;
            if p.id.trim().is_empty() {
                return Err("params.id must not be empty".into());
            }
            Ok(LibraryRequest::Get {
                id: p.id.trim().to_string(),
            })
        }
        "library_random" => {
            let p: RandomParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for library_random: {e}"))?;
            let count = p.count.unwrap_or(1);
            if count == 0 || count > 20 {
                return Err("params.count must be 1..20".into());
            }
            Ok(LibraryRequest::Random {
                kind: normalize_kind(p.kind),
                count,
            })
        }
        "library_recent" => {
            let p: RecentParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for library_recent: {e}"))?;
            let limit = p.limit.unwrap_or(20);
            if limit == 0 || limit > 100 {
                return Err("params.limit must be 1..100".into());
            }
            Ok(LibraryRequest::Recent {
                kind: normalize_kind(p.kind),
                limit,
            })
        }
        "library_stats" => Ok(LibraryRequest::Stats),
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_defaults() {
        let r = parse_request("library_search", b"{}").unwrap();
        match r {
            LibraryRequest::Search { kind, limit, .. } => {
                assert_eq!(kind, "all");
                assert_eq!(limit, 20);
            }
            _ => panic!(),
        }
    }
    #[test]
    fn random_kind_normalized() {
        let r = parse_request("library_random", br#"{"kind":"AUDIO"}"#).unwrap();
        match r {
            LibraryRequest::Random { kind, .. } => assert_eq!(kind, "audio"),
            _ => panic!(),
        }
    }
}
