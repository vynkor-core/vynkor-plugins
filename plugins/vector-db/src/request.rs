use serde::Deserialize;
use serde_json::Value;

#[derive(Debug)]
pub enum VectorRequest {
    Upsert {
        collection: String,
        id: String,
        text: Option<String>,
        vector: Option<Vec<f32>>,
        metadata: Option<Value>,
    },
    BatchUpsert {
        collection: String,
        docs: Vec<BatchDoc>,
    },
    Query {
        collection: String,
        text: Option<String>,
        vector: Option<Vec<f32>>,
        top_k: usize,
        include_vector: bool,
        filter: Option<Value>,
    },
    Get {
        collection: String,
        id: String,
    },
    Delete {
        collection: String,
        id: String,
    },
    List {
        prefix: String,
    },
    Stats {
        collection: String,
    },
    Embed {
        texts: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct BatchDoc {
    pub id: String,
    pub text: Option<String>,
    pub vector: Option<Vec<f32>>,
    pub metadata: Option<Value>,
}

#[derive(Deserialize)]
struct UpsertParams {
    collection: String,
    id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    vector: Option<Vec<f32>>,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Deserialize)]
struct QueryParams {
    collection: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    vector: Option<Vec<f32>>,
    #[serde(default = "default_top_k")]
    top_k: Option<usize>,
    #[serde(default)]
    include_vector: Option<bool>,
    #[serde(default)]
    filter: Option<Value>,
}

fn default_top_k() -> Option<usize> {
    Some(5)
}

#[derive(Deserialize)]
struct GetParams {
    collection: String,
    id: String,
}

#[derive(Deserialize)]
struct DeleteParams {
    collection: String,
    id: String,
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    prefix: String,
}

#[derive(Deserialize)]
struct BatchDocRaw {
    id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    vector: Option<Vec<f32>>,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Deserialize)]
struct BatchUpsertParams {
    collection: String,
    docs: Vec<BatchDocRaw>,
}

#[derive(Deserialize)]
struct EmbedParams {
    texts: Vec<String>,
}

#[derive(Deserialize)]
struct StatsParams {
    collection: String,
}

fn require_nonempty(s: String, field: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        Err(format!("params.{field} must be a non-empty string"))
    } else {
        Ok(s)
    }
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<VectorRequest, String> {
    match action {
        "vec_upsert" => {
            let p: UpsertParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for vec_upsert: {e}"))?;
            let collection = require_nonempty(p.collection, "collection")?;
            let id = require_nonempty(p.id, "id")?;
            if collection.len() > 128 {
                return Err("params.collection too long (max 128)".to_string());
            }
            if id.len() > 256 {
                return Err("params.id too long (max 256)".to_string());
            }
            let has_text = p.text.as_ref().map_or(false, |t| !t.trim().is_empty());
            let has_vector = p.vector.as_ref().map_or(false, |v| !v.is_empty());
            if !has_text && !has_vector {
                return Err("vec_upsert requires at least one of text or vector".to_string());
            }
            if let Some(t) = &p.text {
                if t.len() > 10000 {
                    return Err("params.text too long (max 10000)".to_string());
                }
            }
            if let Some(v) = &p.vector {
                if v.is_empty() {
                    return Err("params.vector must be non-empty".to_string());
                }
                if v.len() > 4096 {
                    return Err(format!("params.vector dim too large: {} > 4096", v.len()));
                }
            }
            Ok(VectorRequest::Upsert {
                collection,
                id,
                text: p.text.filter(|t| !t.trim().is_empty()),
                vector: p.vector,
                metadata: p.metadata,
            })
        }
        "vec_query" => {
            let p: QueryParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for vec_query: {e}"))?;
            let collection = require_nonempty(p.collection, "collection")?;
            let has_text = p.text.as_ref().map_or(false, |t| !t.trim().is_empty());
            let has_vector = p.vector.as_ref().map_or(false, |v| !v.is_empty());
            if !has_text && !has_vector {
                return Err("vec_query requires at least one of text or vector".to_string());
            }
            let top_k = p.top_k.unwrap_or(5);
            if top_k == 0 || top_k > 100 {
                return Err("params.top_k must be 1..100".to_string());
            }
            if let Some(v) = &p.vector {
                if v.is_empty() {
                    return Err("params.vector must be non-empty".to_string());
                }
                if v.len() > 4096 {
                    return Err(format!("params.vector dim too large: {} > 4096", v.len()));
                }
            }
            Ok(VectorRequest::Query {
                collection,
                text: p.text.filter(|t| !t.trim().is_empty()),
                vector: p.vector,
                top_k,
                include_vector: p.include_vector.unwrap_or(false),
                filter: p.filter,
            })
        }
        "vec_get" => {
            let p: GetParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for vec_get: {e}"))?;
            Ok(VectorRequest::Get {
                collection: require_nonempty(p.collection, "collection")?,
                id: require_nonempty(p.id, "id")?,
            })
        }
        "vec_delete" => {
            let p: DeleteParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for vec_delete: {e}"))?;
            Ok(VectorRequest::Delete {
                collection: require_nonempty(p.collection, "collection")?,
                id: require_nonempty(p.id, "id")?,
            })
        }
        "vec_list" => {
            let p: ListParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for vec_list: {e}"))?;
            Ok(VectorRequest::List { prefix: p.prefix })
        }
        "vec_stats" => {
            let p: StatsParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for vec_stats: {e}"))?;
            Ok(VectorRequest::Stats {
                collection: require_nonempty(p.collection, "collection")?,
            })
        }
        "vec_embed" => {
            let p: EmbedParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for vec_embed: {e}"))?;
            if p.texts.is_empty() || p.texts.len() > 64 {
                return Err("params.texts must have 1..=64 items".to_string());
            }
            for (i, t) in p.texts.iter().enumerate() {
                if t.trim().is_empty() {
                    return Err(format!("params.texts[{i}] must not be empty"));
                }
                if t.len() > 10000 {
                    return Err(format!("params.texts[{i}] too long (max 10000)"));
                }
            }
            Ok(VectorRequest::Embed { texts: p.texts })
        }
        "vec_upsert_batch" => {
            let p: BatchUpsertParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for vec_upsert_batch: {e}"))?;
            let collection = require_nonempty(p.collection, "collection")?;
            if collection.len() > 128 {
                return Err("params.collection too long (max 128)".to_string());
            }
            if p.docs.is_empty() {
                return Err("params.docs must be non-empty".to_string());
            }
            if p.docs.len() > 1000 {
                return Err("params.docs too large (max 1000)".to_string());
            }
            let mut docs = Vec::with_capacity(p.docs.len());
            for raw in p.docs {
                let id = require_nonempty(raw.id, "docs[].id")?;
                if id.len() > 256 {
                    return Err("params.docs[].id too long (max 256)".to_string());
                }
                let has_text = raw.text.as_ref().map_or(false, |t| !t.trim().is_empty());
                let has_vector = raw.vector.as_ref().map_or(false, |v| !v.is_empty());
                if !has_text && !has_vector {
                    return Err(format!(
                        "docs[{}] requires at least one of text or vector",
                        docs.len()
                    ));
                }
                if let Some(t) = &raw.text {
                    if t.len() > 10000 {
                        return Err("params.docs[].text too long (max 10000)".to_string());
                    }
                }
                if let Some(v) = &raw.vector {
                    if v.is_empty() {
                        return Err("params.docs[].vector must be non-empty".to_string());
                    }
                    if v.len() > 4096 {
                        return Err(format!(
                            "params.docs[].vector dim too large: {} > 4096",
                            v.len()
                        ));
                    }
                }
                docs.push(BatchDoc {
                    id,
                    text: raw.text.filter(|t| !t.trim().is_empty()),
                    vector: raw.vector,
                    metadata: raw.metadata,
                });
            }
            Ok(VectorRequest::BatchUpsert { collection, docs })
        }
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upsert_with_text() {
        let req = parse_request(
            "vec_upsert",
            br#"{"collection":"c","id":"1","text":"hello"}"#,
        )
        .unwrap();
        assert!(matches!(req, VectorRequest::Upsert { .. }));
    }

    #[test]
    fn parses_upsert_with_vector() {
        let req = parse_request(
            "vec_upsert",
            br#"{"collection":"c","id":"1","vector":[0.1,0.2]}"#,
        )
        .unwrap();
        assert!(matches!(req, VectorRequest::Upsert { .. }));
    }

    #[test]
    fn upsert_rejects_no_content() {
        let err = parse_request("vec_upsert", br#"{"collection":"c","id":"1"}"#).unwrap_err();
        assert!(err.contains("text or vector"));
    }

    #[test]
    fn query_rejects_no_content() {
        let err = parse_request("vec_query", br#"{"collection":"c"}"#).unwrap_err();
        assert!(err.contains("text or vector"));
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse_request("vec_unknown", b"{}").is_err());
    }
}
