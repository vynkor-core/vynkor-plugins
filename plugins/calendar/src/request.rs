//! Request parsing/validation for the `calendar` plugin's actions.
//!
//! Pure layer — no IPC here, mirroring the `notes`/`database` convention:
//! every action's params parse into a [`CalendarRequest`] variant or are
//! rejected with a human-readable error naming the expected shape.

use chrono::TimeZone;
use serde::Deserialize;

pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_DESCRIPTION_BYTES: usize = 4096;
pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_BYTES: usize = 64;
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;
/// Reminder lead cap: 10 years in milliseconds. Bounds the value so
/// `start_ms - remind_before_ms` can never meaningfully underflow.
pub const MAX_REMIND_BEFORE_MS: i64 = 315_360_000_000;

#[derive(Debug)]
pub struct NewEvent {
    pub title: String,
    pub description: String,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    pub all_day: bool,
    pub remind_before_ms: Option<i64>,
    pub tags: Vec<String>,
    pub rrule: Option<String>,
}

#[derive(Debug)]
pub struct EventPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub all_day: Option<bool>,
    pub remind_before_ms: Option<i64>,
    pub tags: Option<Vec<String>>,
    pub rrule: Option<Option<String>>,
}

#[derive(Debug)]
pub enum CalendarRequest {
    Create(NewEvent),
    Get {
        id: String,
    },
    List {
        from_ms: Option<i64>,
        to_ms: Option<i64>,
        tag: Option<String>,
        limit: usize,
        offset: usize,
    },
    Update {
        id: String,
        patch: EventPatch,
    },
    Delete {
        id: String,
    },
    /// `calendar_ics_import {ics_base64}` — decode → parse → upsert by UID.
    Import {
        ics_base64: String,
    },
    /// `calendar_ics_export {}` — all events as one VCALENDAR.
    Export,
}

#[derive(Deserialize)]
struct CreateParams {
    title: String,
    #[serde(default)]
    description: String,
    start_ms: i64,
    #[serde(default)]
    end_ms: Option<i64>,
    #[serde(default)]
    all_day: bool,
    #[serde(default)]
    remind_before_ms: Option<i64>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    rrule: Option<String>,
}

#[derive(Deserialize)]
struct IdParams {
    id: String,
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    from_ms: Option<i64>,
    #[serde(default)]
    to_ms: Option<i64>,
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
    description: Option<String>,
    #[serde(default)]
    start_ms: Option<i64>,
    #[serde(default)]
    end_ms: Option<i64>,
    #[serde(default)]
    all_day: Option<bool>,
    #[serde(default)]
    remind_before_ms: Option<i64>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    rrule: Option<Option<String>>,
}

/// ~6 MiB of decoded ICS; generous for a personal calendar, tight enough to
/// keep one action from flooding `database`.
pub const MAX_ICS_BASE64_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct ImportParams {
    ics_base64: String,
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

fn validate_times(
    start_ms: i64,
    end_ms: Option<i64>,
    remind_before_ms: Option<i64>,
) -> Result<(), String> {
    if start_ms <= 0 {
        return Err("params.start_ms must be a positive unix-milliseconds timestamp".to_string());
    }
    if let Some(end) = end_ms {
        if end < start_ms {
            return Err("params.end_ms must be >= params.start_ms".to_string());
        }
    }
    if let Some(rb) = remind_before_ms {
        if rb < 0 {
            return Err("params.remind_before_ms must be >= 0".to_string());
        }
        if rb > MAX_REMIND_BEFORE_MS {
            return Err(format!(
                "params.remind_before_ms exceeds the {} ms cap",
                MAX_REMIND_BEFORE_MS
            ));
        }
    }
    Ok(())
}

/// Trim entries, drop empties, dedupe preserving first occurrence, enforce
/// caps (same semantics as `notes`).
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

fn validate_rrule(rrule: &str) -> Result<String, String> {
    let s = rrule.trim();
    if s.is_empty() {
        return Err("params.rrule must not be empty when present".into());
    }
    if s.len() > 256 {
        return Err("params.rrule exceeds 256 bytes".into());
    }
    if !s.contains("FREQ=") {
        return Err("params.rrule must contain FREQ= (e.g. FREQ=WEEKLY;BYDAY=MO)".into());
    }
    // Try to parse with rrule crate for early validation using start time 0
    // Use rrule::RRuleSet::from_str style - try parsing
    // For validation we attempt to build an RRule from the string with a dummy DTSTART
    use std::str::FromStr;
    let dtstart = chrono::Utc.timestamp_millis_opt(0).single().unwrap();
    let rrule_str = format!("DTSTART:19700101T000000Z\nRRULE:{}", s);
    if let Err(e) = rrule::RRuleSet::from_str(&rrule_str) {
        return Err(format!("params.rrule invalid: {e}"));
    }
    let _ = dtstart;
    Ok(s.to_string())
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<CalendarRequest, String> {
    match action {
        "event_create" => {
            let p: CreateParams = serde_json::from_slice(params_json).map_err(|e| {
                format!(
                    "invalid params for event_create, expected \
                     {{title, description?, start_ms, end_ms?, all_day?, remind_before_ms?, \
                     tags?}}: {e}"
                )
            })?;
            if p.title.trim().is_empty() {
                return Err("event_create requires a non-empty title".to_string());
            }
            check_size("title", &p.title, MAX_TITLE_BYTES)?;
            check_size("description", &p.description, MAX_DESCRIPTION_BYTES)?;
            validate_times(p.start_ms, p.end_ms, p.remind_before_ms)?;
            let rrule = match p.rrule {
                Some(s) => Some(validate_rrule(&s)?),
                None => None,
            };
            Ok(CalendarRequest::Create(NewEvent {
                title: p.title,
                description: p.description,
                start_ms: p.start_ms,
                end_ms: p.end_ms,
                all_day: p.all_day,
                remind_before_ms: p.remind_before_ms,
                tags: sanitize_tags(&p.tags)?,
                rrule,
            }))
        }
        "event_get" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for event_get, expected {{id}}: {e}"))?;
            Ok(CalendarRequest::Get {
                id: require_nonempty_id(p.id)?,
            })
        }
        "event_list" => {
            let p: ListParams = serde_json::from_slice(params_json).map_err(|e| {
                format!(
                    "invalid params for event_list, expected \
                     {{from_ms?, to_ms?, tag?, limit?, offset?}}: {e}"
                )
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
            Ok(CalendarRequest::List {
                from_ms: p.from_ms,
                to_ms: p.to_ms,
                tag,
                limit,
                offset,
            })
        }
        "event_update" => {
            let p: UpdateParams = serde_json::from_slice(params_json).map_err(|e| {
                format!(
                    "invalid params for event_update, expected \
                     {{id, title?, description?, start_ms?, end_ms?, all_day?, \
                     remind_before_ms?, tags?}}: {e}"
                )
            })?;
            let id = require_nonempty_id(p.id)?;
            let rrule_patch = match p.rrule {
                Some(None) => Some(None),
                Some(Some(s)) => {
                    if s.trim().is_empty() {
                        Some(None)
                    } else {
                        Some(Some(validate_rrule(&s)?))
                    }
                }
                None => None,
            };
            let patch = EventPatch {
                title: p.title,
                description: p.description,
                start_ms: p.start_ms,
                end_ms: p.end_ms,
                all_day: p.all_day,
                remind_before_ms: p.remind_before_ms,
                tags: p.tags,
                rrule: rrule_patch,
            };
            if patch.title.is_none()
                && patch.description.is_none()
                && patch.start_ms.is_none()
                && patch.end_ms.is_none()
                && patch.all_day.is_none()
                && patch.remind_before_ms.is_none()
                && patch.tags.is_none()
                && patch.rrule.is_none()
            {
                return Err(
                    "event_update requires at least one of title, description, start_ms, \
                     end_ms, all_day, remind_before_ms, tags, rrule"
                        .to_string(),
                );
            }
            if let Some(title) = &patch.title {
                if title.trim().is_empty() {
                    return Err("params.title must be non-empty".to_string());
                }
                check_size("title", title, MAX_TITLE_BYTES)?;
            }
            if let Some(description) = &patch.description {
                check_size("description", description, MAX_DESCRIPTION_BYTES)?;
            }
            if let Some(start_ms) = patch.start_ms {
                if start_ms <= 0 {
                    return Err(
                        "params.start_ms must be a positive unix-milliseconds timestamp"
                            .to_string(),
                    );
                }
            }
            if let Some(rb) = patch.remind_before_ms {
                if rb > MAX_REMIND_BEFORE_MS {
                    return Err(format!(
                        "params.remind_before_ms exceeds the {} ms cap",
                        MAX_REMIND_BEFORE_MS
                    ));
                }
            }
            let tags = match patch.tags {
                Some(raw) => Some(sanitize_tags(&raw)?),
                None => None,
            };
            Ok(CalendarRequest::Update {
                id,
                patch: EventPatch { tags, ..patch },
            })
        }
        "event_delete" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for event_delete, expected {{id}}: {e}"))?;
            Ok(CalendarRequest::Delete {
                id: require_nonempty_id(p.id)?,
            })
        }
        "calendar_ics_import" => {
            let p: ImportParams = serde_json::from_slice(params_json).map_err(|e| {
                format!(
                    "invalid params for calendar_ics_import, expected \
                     {{ics_base64}}: {e}"
                )
            })?;
            if p.ics_base64.len() > MAX_ICS_BASE64_BYTES {
                return Err(format!(
                    "params.ics_base64 exceeds {} bytes",
                    MAX_ICS_BASE64_BYTES
                ));
            }
            Ok(CalendarRequest::Import {
                ics_base64: p.ics_base64,
            })
        }
        "calendar_ics_export" => {
            serde_json::from_slice::<serde::de::IgnoredAny>(params_json).map_err(|e| {
                format!("invalid params for calendar_ics_export, expected {{}}: {e}")
            })?;
            Ok(CalendarRequest::Export)
        }
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn create_params(v: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn parses_create_minimal() {
        let req = parse_request(
            "event_create",
            &create_params(json!({"title": "t", "start_ms": 100})),
        )
        .unwrap();
        match req {
            CalendarRequest::Create(e) => {
                assert_eq!(e.title, "t");
                assert_eq!(e.description, "");
                assert_eq!(e.start_ms, 100);
                assert_eq!(e.end_ms, None);
                assert!(!e.all_day);
                assert_eq!(e.remind_before_ms, None);
                assert!(e.tags.is_empty());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn create_requires_nonempty_title() {
        let err = parse_request(
            "event_create",
            &create_params(json!({"title": "  ", "start_ms": 100})),
        )
        .unwrap_err();
        assert!(err.contains("non-empty title"), "error was: {err}");
    }

    #[test]
    fn create_rejects_bad_times() {
        let err = parse_request(
            "event_create",
            &create_params(json!({"title": "t", "start_ms": 0})),
        )
        .unwrap_err();
        assert!(err.contains("start_ms"), "error was: {err}");

        let err = parse_request(
            "event_create",
            &create_params(json!({"title": "t", "start_ms": 200, "end_ms": 100})),
        )
        .unwrap_err();
        assert!(err.contains("end_ms"), "error was: {err}");

        let err = parse_request(
            "event_create",
            &create_params(json!({"title": "t", "start_ms": 200, "remind_before_ms": -1})),
        )
        .unwrap_err();
        assert!(err.contains("remind_before_ms"), "error was: {err}");

        let err = parse_request(
            "event_create",
            &create_params(json!({"title": "t", "start_ms": 200, "remind_before_ms":
            MAX_REMIND_BEFORE_MS + 1})),
        )
        .unwrap_err();
        assert!(err.contains("cap"), "error was: {err}");
    }

    #[test]
    fn list_defaults_and_range_fields() {
        match parse_request("event_list", b"{}").unwrap() {
            CalendarRequest::List {
                from_ms,
                to_ms,
                tag,
                limit,
                offset,
            } => {
                assert_eq!(from_ms, None);
                assert_eq!(to_ms, None);
                assert_eq!(tag, None);
                assert_eq!(limit, DEFAULT_LIMIT);
                assert_eq!(offset, 0);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        match parse_request(
            "event_list",
            &create_params(json!({"from_ms": 5, "to_ms": 9, "tag": " work "})),
        )
        .unwrap()
        {
            CalendarRequest::List {
                from_ms,
                to_ms,
                tag,
                ..
            } => {
                assert_eq!(from_ms, Some(5));
                assert_eq!(to_ms, Some(9));
                assert_eq!(tag, Some("work".to_string()));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn list_rejects_bad_limit() {
        let err = parse_request("event_list", &create_params(json!({"limit": 501}))).unwrap_err();
        assert!(err.contains("limit"), "error was: {err}");
    }

    #[test]
    fn update_requires_at_least_one_field_and_validates_values() {
        let err = parse_request("event_update", &create_params(json!({"id": "1"}))).unwrap_err();
        assert!(err.contains("at least one of"), "error was: {err}");

        let err = parse_request(
            "event_update",
            &create_params(json!({"id": "1", "title": " "})),
        )
        .unwrap_err();
        assert!(err.contains("non-empty"), "error was: {err}");

        match parse_request(
            "event_update",
            &create_params(json!({"id": "1", "remind_before_ms": 0, "all_day": true})),
        )
        .unwrap()
        {
            CalendarRequest::Update { id, patch } => {
                assert_eq!(id, "1");
                assert_eq!(patch.remind_before_ms, Some(0));
                assert_eq!(patch.all_day, Some(true));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn get_and_delete_require_nonempty_id() {
        let err = parse_request("event_get", &create_params(json!({"id": ""}))).unwrap_err();
        assert!(err.contains("non-empty"), "error was: {err}");
        match parse_request("event_delete", &create_params(json!({"id": "3"}))).unwrap() {
            CalendarRequest::Delete { id } => assert_eq!(id, "3"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_action_and_malformed_json() {
        let err = parse_request("event_frobnicate", b"{}").unwrap_err();
        assert!(err.contains("unknown action"), "error was: {err}");
        let err = parse_request("event_create", b"{").unwrap_err();
        assert!(err.contains("invalid params"), "error was: {err}");
    }
}
