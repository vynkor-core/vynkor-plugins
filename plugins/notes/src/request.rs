//! Request parsing/validation for the `notes` plugin's actions.
//!
//! Pure layer — no IPC here. Every action's params are parsed into a
//! [`NotesRequest`] variant or rejected with a human-readable error naming
//! the expected shape (mirrors the `database` plugin's convention so callers
//! see consistent errors across the storage stack).

use serde::Deserialize;

pub const MAX_TITLE_BYTES: usize = 512;
/// 256 KiB — deliberate headroom under `database`'s default 1 MiB
/// `max_value_bytes`, so an oversized body is rejected here with a clear
/// message instead of surfacing as a database-side cap error.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_BYTES: usize = 64;
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;

#[derive(Debug)]
pub enum NotesRequest {
    Create {
        title: String,
        body: String,
        tags: Vec<String>,
    },
    Get {
        id: String,
    },
    List {
        tag: Option<String>,
        limit: usize,
        offset: usize,
    },
    Update {
        id: String,
        title: Option<String>,
        body: Option<String>,
        tags: Option<Vec<String>>,
    },
    Delete {
        id: String,
    },
}

#[derive(Deserialize)]
struct CreateParams {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct IdParams {
    id: String,
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Deserialize)]
struct UpdateParams {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

fn check_size(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        Err(format!(
            "params.{field} exceeds {max} bytes (got {})",
            value.len()
        ))
    } else {
        Ok(())
    }
}

fn require_nonempty_id(id: String) -> Result<String, String> {
    if id.is_empty() {
        Err("params.id must be a non-empty string".to_string())
    } else {
        Ok(id)
    }
}

/// Trim entries, drop empties, dedupe preserving first occurrence, enforce
/// caps. Empty-after-trim entries are skipped rather than rejected — callers
/// building tag arrays programmatically shouldn't fail on `["a", "", "b"]`.
pub fn sanitize_tags(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for tag in raw {
        let tag = tag.trim();
        if tag.is_empty() {
            continue;
        }
        if tag.len() > MAX_TAG_BYTES {
            return Err(format!(
                "params.tags contains a tag longer than {MAX_TAG_BYTES} bytes"
            ));
        }
        if out.iter().any(|t| t == tag) {
            continue;
        }
        if out.len() == MAX_TAGS {
            return Err(format!("params.tags exceeds {MAX_TAGS} unique tags"));
        }
        out.push(tag.to_string());
    }
    Ok(out)
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<NotesRequest, String> {
    match action {
        "note_create" => {
            let p: CreateParams = serde_json::from_slice(params_json).map_err(|e| {
                format!("invalid params for note_create, expected {{title?, body?, tags?}}: {e}")
            })?;
            if p.title.trim().is_empty() && p.body.trim().is_empty() {
                return Err("note_create requires a non-empty title or body".to_string());
            }
            check_size("title", &p.title, MAX_TITLE_BYTES)?;
            check_size("body", &p.body, MAX_BODY_BYTES)?;
            Ok(NotesRequest::Create {
                title: p.title,
                body: p.body,
                tags: sanitize_tags(&p.tags)?,
            })
        }
        "note_get" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for note_get, expected {{id}}: {e}"))?;
            Ok(NotesRequest::Get {
                id: require_nonempty_id(p.id)?,
            })
        }
        "note_list" => {
            let p: ListParams = serde_json::from_slice(params_json).map_err(|e| {
                format!("invalid params for note_list, expected {{tag?, limit?, offset?}}: {e}")
            })?;
            let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
            if limit == 0 || limit > MAX_LIMIT {
                return Err(format!("params.limit must be between 1 and {MAX_LIMIT}"));
            }
            let offset = p.offset.unwrap_or(0);
            let tag = match p.tag {
                Some(t) => {
                    let t = t.trim().to_string();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                }
                None => None,
            };
            Ok(NotesRequest::List { tag, limit, offset })
        }
        "note_update" => {
            let p: UpdateParams = serde_json::from_slice(params_json).map_err(|e| {
                format!(
                    "invalid params for note_update, expected {{id, title?, body?, tags?}}: {e}"
                )
            })?;
            let id = require_nonempty_id(p.id)?;
            if p.title.is_none() && p.body.is_none() && p.tags.is_none() {
                return Err("note_update requires at least one of title, body, tags".to_string());
            }
            if let Some(title) = &p.title {
                check_size("title", title, MAX_TITLE_BYTES)?;
            }
            if let Some(body) = &p.body {
                check_size("body", body, MAX_BODY_BYTES)?;
            }
            let tags = match p.tags {
                Some(raw) => Some(sanitize_tags(&raw)?),
                None => None,
            };
            Ok(NotesRequest::Update {
                id,
                title: p.title,
                body: p.body,
                tags,
            })
        }
        "note_delete" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for note_delete, expected {{id}}: {e}"))?;
            Ok(NotesRequest::Delete {
                id: require_nonempty_id(p.id)?,
            })
        }
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_create_with_defaults() {
        let req = parse_request("note_create", br#"{"body": "hello"}"#).unwrap();
        match req {
            NotesRequest::Create { title, body, tags } => {
                assert_eq!(title, "");
                assert_eq!(body, "hello");
                assert!(tags.is_empty());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn create_rejects_empty_title_and_body() {
        let err = parse_request("note_create", br#"{"title": "  ", "body": ""}"#).unwrap_err();
        assert!(err.contains("non-empty title or body"), "error was: {err}");
    }

    #[test]
    fn create_rejects_oversized_title_and_body() {
        let big_title = format!(r#"{{"title": "{}"}}"#, "x".repeat(MAX_TITLE_BYTES + 1));
        let err = parse_request("note_create", big_title.as_bytes()).unwrap_err();
        assert!(err.contains("params.title exceeds"), "error was: {err}");

        let big_body = format!(
            r#"{{"title": "t", "body": "{}"}}"#,
            "x".repeat(MAX_BODY_BYTES + 1)
        );
        let err = parse_request("note_create", big_body.as_bytes()).unwrap_err();
        assert!(err.contains("params.body exceeds"), "error was: {err}");
    }

    #[test]
    fn tags_are_trimmed_deduped_and_capped() {
        let params = serde_json::json!({
            "body": "b",
            "tags": [" work ", "", "work", "urgent"]
        });
        let raw = serde_json::to_vec(&params).unwrap();
        let req = parse_request("note_create", &raw).unwrap();
        match req {
            NotesRequest::Create { tags, .. } => {
                assert_eq!(tags, vec!["work".to_string(), "urgent".to_string()]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn tags_reject_oversized_entry_and_too_many_unique() {
        let long_tag = "x".repeat(MAX_TAG_BYTES + 1);
        let params = serde_json::json!({"body": "b", "tags": [long_tag]});
        let err = parse_request(
            "note_create",
            serde_json::to_vec(&params).unwrap().as_slice(),
        )
        .unwrap_err();
        assert!(err.contains("longer than"), "error was: {err}");

        let many: Vec<String> = (0..=MAX_TAGS).map(|i| format!("t{i}")).collect();
        let params = serde_json::json!({"body": "b", "tags": many});
        let err = parse_request(
            "note_create",
            serde_json::to_vec(&params).unwrap().as_slice(),
        )
        .unwrap_err();
        assert!(err.contains("exceeds"), "error was: {err}");
    }

    #[test]
    fn parses_get_and_delete_with_nonempty_id() {
        match parse_request("note_get", br#"{"id": "7"}"#).unwrap() {
            NotesRequest::Get { id } => assert_eq!(id, "7"),
            other => panic!("wrong variant: {other:?}"),
        }
        match parse_request("note_delete", br#"{"id": "7"}"#).unwrap() {
            NotesRequest::Delete { id } => assert_eq!(id, "7"),
            other => panic!("wrong variant: {other:?}"),
        }
        let err = parse_request("note_get", br#"{"id": ""}"#).unwrap_err();
        assert!(err.contains("non-empty"), "error was: {err}");
    }

    #[test]
    fn list_defaults_limit_and_offset() {
        match parse_request("note_list", b"{}").unwrap() {
            NotesRequest::List { tag, limit, offset } => {
                assert_eq!(tag, None);
                assert_eq!(limit, DEFAULT_LIMIT);
                assert_eq!(offset, 0);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn list_rejects_bad_limit_and_trims_tag() {
        for bad in [0, MAX_LIMIT + 1] {
            let params = serde_json::json!({"limit": bad});
            let err = parse_request("note_list", serde_json::to_vec(&params).unwrap().as_slice())
                .unwrap_err();
            assert!(err.contains("limit"), "error was: {err}");
        }
        match parse_request("note_list", br#"{"tag": " work "}"#).unwrap() {
            NotesRequest::List { tag, .. } => assert_eq!(tag, Some("work".to_string())),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn update_requires_at_least_one_field() {
        let err = parse_request("note_update", br#"{"id": "1"}"#).unwrap_err();
        assert!(err.contains("at least one of"), "error was: {err}");
        match parse_request("note_update", br#"{"id": "1", "tags": ["a"]}"#).unwrap() {
            NotesRequest::Update {
                id,
                title,
                body,
                tags,
            } => {
                assert_eq!(id, "1");
                assert_eq!(title, None);
                assert_eq!(body, None);
                assert_eq!(tags, Some(vec!["a".to_string()]));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_action_and_malformed_json() {
        let err = parse_request("note_frobnicate", b"{}").unwrap_err();
        assert!(err.contains("unknown action"), "error was: {err}");
        let err = parse_request("note_get", b"not json").unwrap_err();
        assert!(err.contains("invalid params"), "error was: {err}");
    }
}
