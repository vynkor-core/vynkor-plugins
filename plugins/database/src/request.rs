use serde::Deserialize;
use serde_json::Value;

#[derive(Debug)]
pub enum DbRequest {
    Get {
        key: String,
    },
    Set {
        key: String,
        value: Value,
        ttl_ms: Option<i64>,
    },
    Delete {
        key: String,
    },
    BatchGet {
        keys: Vec<String>,
    },
    Query {
        sql: String,
        params: Vec<Value>,
    },
    Incr {
        key: String,
        delta: i64,
    },
    Keys {
        prefix: String,
    },
    Append {
        key: String,
        value: Value,
    },
    Patch {
        key: String,
        path: String,
        value: Value,
    },
}

#[derive(Deserialize)]
struct KeyParams {
    key: String,
}

#[derive(Deserialize)]
struct SetParams {
    key: String,
    value: Value,
    #[serde(default)]
    ttl_ms: Option<i64>,
}

#[derive(Deserialize)]
struct BatchGetParams {
    keys: Vec<String>,
}

#[derive(Deserialize)]
struct QueryParams {
    sql: String,
    #[serde(default)]
    params: Vec<Value>,
}

#[derive(Deserialize)]
struct IncrParams {
    key: String,
    #[serde(default = "default_delta")]
    delta: i64,
}

fn default_delta() -> i64 {
    1
}

#[derive(Deserialize)]
struct KeysParams {
    #[serde(default)]
    prefix: String,
}

#[derive(Deserialize)]
struct AppendParams {
    key: String,
    value: Value,
}

#[derive(Deserialize)]
struct PatchParams {
    key: String,
    path: String,
    value: Value,
}

fn require_nonempty_key(key: String) -> Result<String, String> {
    if key.is_empty() {
        Err("params.key must be a non-empty string".to_string())
    } else {
        Ok(key)
    }
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<DbRequest, String> {
    match action {
        "db_get" => {
            let p: KeyParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for db_get, expected {{key}}: {e}"))?;
            Ok(DbRequest::Get {
                key: require_nonempty_key(p.key)?,
            })
        }
        "db_set" => {
            let p: SetParams = serde_json::from_slice(params_json).map_err(|e| {
                format!("invalid params for db_set, expected {{key, value, ttl_ms?}}: {e}")
            })?;
            Ok(DbRequest::Set {
                key: require_nonempty_key(p.key)?,
                value: p.value,
                ttl_ms: p.ttl_ms.filter(|t| *t > 0),
            })
        }
        "db_delete" => {
            let p: KeyParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for db_delete, expected {{key}}: {e}"))?;
            Ok(DbRequest::Delete {
                key: require_nonempty_key(p.key)?,
            })
        }
        "db_batch_get" => {
            let p: BatchGetParams = serde_json::from_slice(params_json).map_err(|e| {
                format!("invalid params for db_batch_get, expected {{keys: [..]}}: {e}")
            })?;
            if p.keys.is_empty() {
                return Err("params.keys must be a non-empty array".to_string());
            }
            Ok(DbRequest::BatchGet { keys: p.keys })
        }
        "db_query" => {
            let p: QueryParams = serde_json::from_slice(params_json).map_err(|e| {
                format!("invalid params for db_query, expected {{sql, params?}}: {e}")
            })?;
            if p.sql.trim().is_empty() {
                return Err("params.sql must be a non-empty string".to_string());
            }
            Ok(DbRequest::Query {
                sql: p.sql,
                params: p.params,
            })
        }
        "db_incr" => {
            let p: IncrParams = serde_json::from_slice(params_json).map_err(|e| {
                format!("invalid params for db_incr, expected {{key, delta?}}: {e}")
            })?;
            Ok(DbRequest::Incr {
                key: require_nonempty_key(p.key)?,
                delta: p.delta,
            })
        }
        "db_keys" => {
            let p: KeysParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for db_keys, expected {{prefix?}}: {e}"))?;
            Ok(DbRequest::Keys { prefix: p.prefix })
        }
        "db_append" => {
            let p: AppendParams = serde_json::from_slice(params_json).map_err(|e| {
                format!("invalid params for db_append, expected {{key, value}}: {e}")
            })?;
            Ok(DbRequest::Append {
                key: require_nonempty_key(p.key)?,
                value: p.value,
            })
        }
        "db_patch" => {
            let p: PatchParams = serde_json::from_slice(params_json).map_err(|e| {
                format!("invalid params for db_patch, expected {{key, path, value}}: {e}")
            })?;
            let key = require_nonempty_key(p.key)?;
            if p.path.is_empty() {
                return Err("params.path must be a non-empty string".to_string());
            }
            Ok(DbRequest::Patch {
                key,
                path: p.path,
                value: p.value,
            })
        }
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_db_get() {
        let req = parse_request("db_get", br#"{"key": "foo"}"#).unwrap();
        match req {
            DbRequest::Get { key } => assert_eq!(key, "foo"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_db_set() {
        let req = parse_request("db_set", br#"{"key": "foo", "value": {"a": 1}}"#).unwrap();
        match req {
            DbRequest::Set { key, value, ttl_ms } => {
                assert_eq!(key, "foo");
                assert_eq!(value, serde_json::json!({"a": 1}));
                assert_eq!(ttl_ms, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn db_set_parses_ttl_ms() {
        let req =
            parse_request("db_set", br#"{"key": "foo", "value": 1, "ttl_ms": 5000}"#).unwrap();
        assert!(matches!(
            req,
            DbRequest::Set {
                ttl_ms: Some(5000),
                ..
            }
        ));
    }

    #[test]
    fn db_set_treats_zero_and_negative_ttl_as_no_expiry() {
        for ttl in [0, -1] {
            let params = format!(r#"{{"key": "k", "value": 1, "ttl_ms": {ttl}}}"#);
            let req = parse_request("db_set", params.as_bytes()).unwrap();
            assert!(
                matches!(req, DbRequest::Set { ttl_ms: None, .. }),
                "ttl_ms {ttl}"
            );
        }
    }

    #[test]
    fn parses_db_incr_with_default_delta() {
        let req = parse_request("db_incr", br#"{"key": "views"}"#).unwrap();
        assert!(matches!(req, DbRequest::Incr { key, delta: 1 } if key == "views"));
    }

    #[test]
    fn parses_db_incr_with_explicit_delta() {
        let req = parse_request("db_incr", br#"{"key": "views", "delta": -3}"#).unwrap();
        assert!(matches!(req, DbRequest::Incr { key, delta: -3 } if key == "views"));
    }

    #[test]
    fn db_incr_zero_delta_is_kept() {
        let req = parse_request("db_incr", br#"{"key": "views", "delta": 0}"#).unwrap();
        assert!(matches!(req, DbRequest::Incr { key, delta: 0 } if key == "views"));
    }

    #[test]
    fn parses_db_keys_with_default_prefix() {
        let req = parse_request("db_keys", b"{}").unwrap();
        assert!(matches!(req, DbRequest::Keys { prefix } if prefix.is_empty()));
    }

    #[test]
    fn parses_db_keys_with_prefix() {
        let req = parse_request("db_keys", br#"{"prefix": "user:"}"#).unwrap();
        assert!(matches!(req, DbRequest::Keys { prefix } if prefix == "user:"));
    }

    #[test]
    fn parses_db_append() {
        let req = parse_request("db_append", br#"{"key": "log", "value": {"n": 1}}"#).unwrap();
        match req {
            DbRequest::Append { key, value } => {
                assert_eq!(key, "log");
                assert_eq!(value, serde_json::json!({"n": 1}));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_db_patch() {
        let req = parse_request(
            "db_patch",
            br#"{"key": "doc", "path": "$.a.b", "value": 5}"#,
        )
        .unwrap();
        match req {
            DbRequest::Patch { key, path, value } => {
                assert_eq!(key, "doc");
                assert_eq!(path, "$.a.b");
                assert_eq!(value, serde_json::json!(5));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn db_patch_rejects_empty_path() {
        let err =
            parse_request("db_patch", br#"{"key": "doc", "path": "", "value": 1}"#).unwrap_err();
        assert!(err.contains("path"), "error was: {err}");
    }

    #[test]
    fn db_patch_rejects_missing_path() {
        let err = parse_request("db_patch", br#"{"key": "doc", "value": 1}"#).unwrap_err();
        assert!(err.contains("path"), "error was: {err}");
    }

    #[test]
    fn parses_db_delete() {
        let req = parse_request("db_delete", br#"{"key": "foo"}"#).unwrap();
        assert!(matches!(req, DbRequest::Delete { key } if key == "foo"));
    }

    #[test]
    fn parses_db_batch_get() {
        let req = parse_request("db_batch_get", br#"{"keys": ["a", "b"]}"#).unwrap();
        match req {
            DbRequest::BatchGet { keys } => {
                assert_eq!(keys, vec!["a".to_string(), "b".to_string()])
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_db_query() {
        let req = parse_request(
            "db_query",
            br#"{"sql": "select * from kv where key = ?1", "params": ["foo"]}"#,
        )
        .unwrap();
        match req {
            DbRequest::Query { sql, params } => {
                assert_eq!(sql, "select * from kv where key = ?1");
                assert_eq!(params, vec![serde_json::json!("foo")]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn db_query_defaults_params_to_empty() {
        let req = parse_request("db_query", br#"{"sql": "select 1"}"#).unwrap();
        assert!(matches!(req, DbRequest::Query { params, .. } if params.is_empty()));
    }

    #[test]
    fn rejects_unknown_action() {
        let err = parse_request("db_frobnicate", b"{}").unwrap_err();
        assert!(err.contains("db_frobnicate"), "error was: {err}");
    }

    #[test]
    fn rejects_malformed_json() {
        let err = parse_request("db_get", b"not json").unwrap_err();
        assert!(err.contains("key"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_key() {
        let err = parse_request("db_get", br#"{"key": ""}"#).unwrap_err();
        assert!(err.contains("key"), "error was: {err}");
    }
}
