//! Request parsing/validation for the `contacts` plugin.

use serde::Deserialize;

pub const MAX_NAME_BYTES: usize = 256;
pub const MAX_EMAIL_BYTES: usize = 256;
pub const MAX_PHONE_BYTES: usize = 64;
pub const MAX_NOTES_BYTES: usize = 4096;
pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_BYTES: usize = 64;
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;

#[derive(Debug)]
pub enum ContactsRequest {
    Create {
        name: String,
        email: String,
        phone: String,
        notes: String,
        tags: Vec<String>,
    },
    Get {
        id: String,
    },
    List {
        query: Option<String>,
        tag: Option<String>,
        limit: usize,
        offset: usize,
    },
    Update {
        id: String,
        name: Option<String>,
        email: Option<String>,
        phone: Option<String>,
        notes: Option<String>,
        tags: Option<Vec<String>>,
    },
    Delete {
        id: String,
    },
}

#[derive(Deserialize)]
struct CreateParams {
    name: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    phone: String,
    #[serde(default)]
    notes: String,
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
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    notes: Option<String>,
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
        Err("params.id must be a non-empty string".into())
    } else {
        Ok(id)
    }
}

fn validate_email(email: &str) -> Result<(), String> {
    if email.is_empty() {
        return Ok(());
    }
    // very light check: contains @ and dot after it, no spaces
    if email.contains(' ') {
        return Err("params.email must not contain spaces".into());
    }
    let parts: Vec<&str> = email.split('@').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() || !parts[1].contains('.') {
        return Err("params.email must be a valid email (user@host.domain)".into());
    }
    Ok(())
}

pub fn sanitize_tags(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for tag in raw {
        let t = tag.trim();
        if t.is_empty() {
            continue;
        }
        if t.len() > MAX_TAG_BYTES {
            return Err(format!(
                "params.tags contains a tag longer than {MAX_TAG_BYTES} bytes"
            ));
        }
        if out.iter().any(|x| x == t) {
            continue;
        }
        if out.len() == MAX_TAGS {
            return Err(format!("params.tags exceeds {MAX_TAGS} unique tags"));
        }
        out.push(t.to_string());
    }
    Ok(out)
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<ContactsRequest, String> {
    match action {
        "contact_create" => {
            let p: CreateParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for contact_create, expected {{name, email?, phone?, notes?, tags?}}: {e}"))?;
            let name = p.name.trim().to_string();
            if name.is_empty() {
                return Err("contact_create requires a non-empty name".into());
            }
            check_size("name", &name, MAX_NAME_BYTES)?;
            check_size("email", &p.email, MAX_EMAIL_BYTES)?;
            check_size("phone", &p.phone, MAX_PHONE_BYTES)?;
            check_size("notes", &p.notes, MAX_NOTES_BYTES)?;
            validate_email(p.email.trim())?;
            Ok(ContactsRequest::Create {
                name,
                email: p.email.trim().to_string(),
                phone: p.phone.trim().to_string(),
                notes: p.notes,
                tags: sanitize_tags(&p.tags)?,
            })
        }
        "contact_get" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for contact_get, expected {{id}}: {e}"))?;
            Ok(ContactsRequest::Get {
                id: require_nonempty_id(p.id)?,
            })
        }
        "contact_list" => {
            let p: ListParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for contact_list, expected {{query?, tag?, limit?, offset?}}: {e}"))?;
            let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
            if limit == 0 || limit > MAX_LIMIT {
                return Err(format!("params.limit must be between 1 and {MAX_LIMIT}"));
            }
            let offset = p.offset.unwrap_or(0);
            let tag = p.tag.and_then(|t| {
                let x = t.trim().to_string();
                if x.is_empty() {
                    None
                } else {
                    Some(x)
                }
            });
            let query = p.query.and_then(|q| {
                let x = q.trim().to_string();
                if x.is_empty() {
                    None
                } else {
                    Some(x)
                }
            });
            Ok(ContactsRequest::List {
                query,
                tag,
                limit,
                offset,
            })
        }
        "contact_update" => {
            let p: UpdateParams = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for contact_update, expected {{id, name?, email?, phone?, notes?, tags?}}: {e}"))?;
            let id = require_nonempty_id(p.id)?;
            if p.name.is_none()
                && p.email.is_none()
                && p.phone.is_none()
                && p.notes.is_none()
                && p.tags.is_none()
            {
                return Err(
                    "contact_update requires at least one of name, email, phone, notes, tags"
                        .into(),
                );
            }
            if let Some(name) = &p.name {
                if name.trim().is_empty() {
                    return Err("params.name must not be empty when provided".into());
                }
                check_size("name", name.trim(), MAX_NAME_BYTES)?;
            }
            if let Some(email) = &p.email {
                check_size("email", email, MAX_EMAIL_BYTES)?;
                validate_email(email.trim())?;
            }
            if let Some(phone) = &p.phone {
                check_size("phone", phone, MAX_PHONE_BYTES)?;
            }
            if let Some(notes) = &p.notes {
                check_size("notes", notes, MAX_NOTES_BYTES)?;
            }
            let tags = match p.tags {
                Some(raw) => Some(sanitize_tags(&raw)?),
                None => None,
            };
            // normalize name/email/phone trimming
            let name = p.name.map(|s| s.trim().to_string());
            let email = p.email.map(|s| s.trim().to_string());
            let phone = p.phone.map(|s| s.trim().to_string());
            Ok(ContactsRequest::Update {
                id,
                name,
                email,
                phone,
                notes: p.notes,
                tags,
            })
        }
        "contact_delete" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for contact_delete, expected {{id}}: {e}"))?;
            Ok(ContactsRequest::Delete {
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
        let r = parse_request("contact_create", br#"{"name":"Alice","email":"a@b.cc"}"#).unwrap();
        match r {
            ContactsRequest::Create { name, email, .. } => {
                assert_eq!(name, "Alice");
                assert_eq!(email, "a@b.cc");
            }
            _ => panic!(),
        }
    }
    #[test]
    fn create_rejects_empty_name() {
        let e = parse_request("contact_create", br#"{"name":"  "}"#).unwrap_err();
        assert!(e.contains("non-empty name"));
    }
    #[test]
    fn email_validation() {
        let e = parse_request("contact_create", br#"{"name":"Bob","email":"bad"}"#).unwrap_err();
        assert!(e.contains("email"));
    }
    #[test]
    fn list_defaults() {
        match parse_request("contact_list", b"{}").unwrap() {
            ContactsRequest::List { limit, offset, .. } => {
                assert_eq!(limit, 100);
                assert_eq!(offset, 0);
            }
            _ => panic!(),
        }
    }
    #[test]
    fn update_requires_field() {
        let e = parse_request("contact_update", br#"{"id":"1"}"#).unwrap_err();
        assert!(e.contains("at least one"));
    }
}
