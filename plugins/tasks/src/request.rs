//! Request parsing for tasks plugin.

use serde::Deserialize;

pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_NOTES_BYTES: usize = 4096;
pub const MAX_LIST_BYTES: usize = 64;
pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_BYTES: usize = 64;
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;

#[derive(Debug)]
pub enum TasksRequest {
    Create {
        title: String,
        notes: String,
        list: String,
        due_ms: Option<i64>,
        tags: Vec<String>,
    },
    Get {
        id: String,
    },
    List {
        query: Option<String>,
        list: Option<String>,
        status: Option<String>,
        tag: Option<String>,
        limit: usize,
        offset: usize,
    },
    Update {
        id: String,
        title: Option<String>,
        notes: Option<String>,
        list: Option<String>,
        due_ms: Option<Option<i64>>,
        tags: Option<Vec<String>>,
        done: Option<bool>,
    },
    Done {
        id: String,
        done: bool,
    },
    Delete {
        id: String,
    },
}

#[derive(Deserialize)]
struct CreateParams {
    title: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    list: Option<String>,
    #[serde(default)]
    due_ms: Option<i64>,
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
    query: Option<String>,
    #[serde(default)]
    list: Option<String>,
    #[serde(default)]
    status: Option<String>,
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
    notes: Option<String>,
    #[serde(default)]
    list: Option<String>,
    #[serde(default)]
    due_ms: Option<Option<i64>>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    done: Option<bool>,
}

#[derive(Deserialize)]
struct DoneParams {
    id: String,
    #[serde(default)]
    done: Option<bool>,
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
        Err("params.id must be non-empty".into())
    } else {
        Ok(id)
    }
}
pub fn sanitize_tags(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for tag in raw {
        let t = tag.trim();
        if t.is_empty() {
            continue;
        }
        if t.len() > MAX_TAG_BYTES {
            return Err(format!("params.tags tag > {MAX_TAG_BYTES} bytes"));
        }
        if out.iter().any(|x| x == t) {
            continue;
        }
        if out.len() == MAX_TAGS {
            return Err(format!("params.tags exceeds {MAX_TAGS}"));
        }
        out.push(t.to_string());
    }
    Ok(out)
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<TasksRequest, String> {
    match action {
        "task_create" => {
            let p: CreateParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for task_create: {e}"))?;
            let title = p.title.trim().to_string();
            if title.is_empty() {
                return Err("task_create requires non-empty title".into());
            }
            check_size("title", &title, MAX_TITLE_BYTES)?;
            check_size("notes", &p.notes, MAX_NOTES_BYTES)?;
            let list = p.list.unwrap_or_else(|| "default".into());
            let list = list.trim().to_string();
            if list.is_empty() {
                return Err("params.list must not be empty when present".into());
            }
            check_size("list", &list, MAX_LIST_BYTES)?;
            if let Some(due) = p.due_ms {
                if due < 0 {
                    return Err("params.due_ms must be >=0".into());
                }
            }
            Ok(TasksRequest::Create {
                title,
                notes: p.notes,
                list,
                due_ms: p.due_ms,
                tags: sanitize_tags(&p.tags)?,
            })
        }
        "task_get" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for task_get: {e}"))?;
            Ok(TasksRequest::Get {
                id: require_nonempty_id(p.id)?,
            })
        }
        "task_list" => {
            let p: ListParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for task_list: {e}"))?;
            let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
            if limit == 0 || limit > MAX_LIMIT {
                return Err(format!("params.limit 1..{MAX_LIMIT}"));
            }
            let offset = p.offset.unwrap_or(0);
            let list = p.list.and_then(|s| {
                let t = s.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            });
            let status = p.status.and_then(|s| {
                let t = s.trim().to_lowercase();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            });
            if let Some(ref s) = status {
                if s != "pending" && s != "done" && s != "all" {
                    return Err("params.status must be pending|done|all".into());
                }
            }
            let tag = p.tag.and_then(|s| {
                let t = s.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            });
            let query = p.query.and_then(|s| {
                let t = s.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            });
            Ok(TasksRequest::List {
                query,
                list,
                status,
                tag,
                limit,
                offset,
            })
        }
        "task_update" => {
            let p: UpdateParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for task_update: {e}"))?;
            let id = require_nonempty_id(p.id)?;
            if p.title.is_none()
                && p.notes.is_none()
                && p.list.is_none()
                && p.due_ms.is_none()
                && p.tags.is_none()
                && p.done.is_none()
            {
                return Err(
                    "task_update requires at least one of title, notes, list, due_ms, tags, done"
                        .into(),
                );
            }
            if let Some(t) = &p.title {
                if t.trim().is_empty() {
                    return Err("params.title must not be empty".into());
                }
                check_size("title", t.trim(), MAX_TITLE_BYTES)?;
            }
            if let Some(n) = &p.notes {
                check_size("notes", n, MAX_NOTES_BYTES)?;
            }
            if let Some(l) = &p.list {
                if l.trim().is_empty() {
                    return Err("params.list must not be empty".into());
                }
                check_size("list", l.trim(), MAX_LIST_BYTES)?;
            }
            if let Some(Some(due)) = p.due_ms {
                if due < 0 {
                    return Err("params.due_ms must be >=0".into());
                }
            }
            let tags = match p.tags {
                Some(raw) => Some(sanitize_tags(&raw)?),
                None => None,
            };
            let title = p.title.map(|s| s.trim().to_string());
            let list = p.list.map(|s| s.trim().to_string());
            Ok(TasksRequest::Update {
                id,
                title,
                notes: p.notes,
                list,
                due_ms: p.due_ms,
                tags,
                done: p.done,
            })
        }
        "task_done" => {
            let p: DoneParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for task_done: {e}"))?;
            Ok(TasksRequest::Done {
                id: require_nonempty_id(p.id)?,
                done: p.done.unwrap_or(true),
            })
        }
        "task_delete" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for task_delete: {e}"))?;
            Ok(TasksRequest::Delete {
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
    fn create_ok() {
        let r = parse_request("task_create", br#"{"title":"Buy milk"}"#).unwrap();
        match r {
            TasksRequest::Create { title, .. } => assert_eq!(title, "Buy milk"),
            _ => panic!(),
        }
    }
    #[test]
    fn create_rejects_empty() {
        let e = parse_request("task_create", br#"{"title":"  "}"#).unwrap_err();
        assert!(e.contains("non-empty"));
    }
    #[test]
    fn list_defaults() {
        match parse_request("task_list", b"{}").unwrap() {
            TasksRequest::List { limit, .. } => assert_eq!(limit, 100),
            _ => panic!(),
        }
    }
    #[test]
    fn done_defaults_true() {
        match parse_request("task_done", br#"{"id":"1"}"#).unwrap() {
            TasksRequest::Done { done, .. } => assert!(done),
            _ => panic!(),
        }
    }
    #[test]
    fn update_requires_field() {
        let e = parse_request("task_update", br#"{"id":"1"}"#).unwrap_err();
        assert!(e.contains("at least one"));
    }
}
