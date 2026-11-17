//! Request parsing/validation for the `scheduler` plugin's actions.
//!
//! Pure layer — no IPC here, mirroring the `notes`/`calendar` convention:
//! every action's params parse into a [`SchedulerRequest`] variant or are
//! rejected with a human-readable error naming the expected shape. Cron
//! expressions are validated up front (`schedule_set` never persists
//! something the scan loop would silently skip later).

use serde::Deserialize;
use serde_json::Value;

use crate::model::{validate_cron_expr_tz, Fire, Trigger};

pub const MAX_NAME_BYTES: usize = 256;
pub const MAX_CRON_BYTES: usize = 128;
pub const MAX_ACTION_NAME_BYTES: usize = 128;
pub const MAX_ID_BYTES: usize = 64;
/// Serialized JSON cap shared by event payloads and action params.
pub const MAX_JSON_BYTES: usize = 65_536;
/// Delay cap: 10 years in milliseconds.
pub const MAX_DELAY_MS: i64 = 315_360_000_000;
const MIN_TZ_OFFSET_MIN: i32 = -720;
const MAX_TZ_OFFSET_MIN: i32 = 840;
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;

#[derive(Debug)]
pub struct NewSchedule {
    pub id: Option<String>,
    pub name: Option<String>,
    pub enabled: bool,
    /// Stored trigger with any relative delay already resolved against
    /// `now_ms` (the caller supplies it — request parsing stays pure).
    pub trigger: Trigger,
    pub fire: Fire,
}

#[derive(Debug)]
pub enum SchedulerRequest {
    Set(NewSchedule),
    Get { id: String },
    List { limit: usize, offset: usize },
    Delete { id: String },
}

#[derive(Deserialize)]
struct SetParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    once: Option<OnceParams>,
    #[serde(default)]
    cron: Option<CronParams>,
    #[serde(default)]
    event: Option<EventFire>,
    #[serde(default)]
    action: Option<ActionFire>,
}

#[derive(Deserialize)]
struct OnceParams {
    #[serde(default)]
    at_ms: Option<i64>,
    #[serde(default)]
    delay_ms: Option<i64>,
}

#[derive(Deserialize)]
struct CronParams {
    expr: String,
    #[serde(default)]
    tz_offset_min: i32,
    #[serde(default)]
    tz: Option<String>,
}

#[derive(Deserialize)]
struct EventFire {
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct ActionFire {
    name: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
struct IdParams {
    id: String,
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
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

fn check_json_size(field: &str, value: &Value, max: usize) -> Result<(), String> {
    let serialized = serde_json::to_string(value)
        .map_err(|e| format!("params.{field} is not valid JSON: {e}"))?;
    if serialized.len() > max {
        Err(format!(
            "params.{field} serializes to more than {max} bytes (got {})",
            serialized.len()
        ))
    } else {
        Ok(())
    }
}

fn require_nonempty_id(id: String) -> Result<String, String> {
    if id.is_empty() {
        Err("params.id must be a non-empty string".to_string())
    } else if id.len() > MAX_ID_BYTES {
        Err(format!("params.id exceeds {MAX_ID_BYTES} bytes"))
    } else {
        Ok(id)
    }
}

/// Client-supplied ids double as database keys (`sched:<id>`) and log
/// prefixes, so they stay on `[A-Za-z0-9_-]`.
fn validate_client_id(id: &str) -> Result<(), String> {
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("params.id may only contain ASCII letters, digits, '_' and '-'".to_string());
    }
    Ok(())
}

fn parse_trigger(p: &SetParams, now_ms: i64) -> Result<Trigger, String> {
    let once = p.once.as_ref();
    let cron = p.cron.as_ref();
    match (once, cron) {
        (Some(_), Some(_)) => {
            Err("schedule_set requires exactly one of params.once or params.cron".to_string())
        }
        (None, None) => {
            Err("schedule_set requires exactly one of params.once or params.cron".to_string())
        }
        (Some(o), None) => match (o.at_ms, o.delay_ms) {
            (Some(_), Some(_)) => {
                Err("params.once requires exactly one of at_ms or delay_ms".to_string())
            }
            (Some(at_ms), None) => {
                if at_ms <= 0 {
                    return Err("params.once.at_ms must be a positive unix-milliseconds \
                                    timestamp"
                        .to_string());
                }
                Ok(Trigger::Once { at_ms })
            }
            (None, Some(delay_ms)) => {
                if delay_ms < 0 {
                    return Err("params.once.delay_ms must be >= 0".to_string());
                }
                if delay_ms > MAX_DELAY_MS {
                    return Err(format!(
                        "params.once.delay_ms exceeds the {MAX_DELAY_MS} ms cap"
                    ));
                }
                Ok(Trigger::Once {
                    at_ms: now_ms.saturating_add(delay_ms),
                })
            }
            (None, None) => {
                Err("params.once requires exactly one of at_ms or delay_ms".to_string())
            }
        },
        (None, Some(c)) => {
            check_size("cron.expr", &c.expr, MAX_CRON_BYTES)?;
            let tz = match &c.tz {
                Some(s) => {
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        return Err("params.cron.tz must not be empty when present".into());
                    }
                    if trimmed.len() > 64 {
                        return Err("params.cron.tz exceeds 64 bytes".into());
                    }
                    Some(trimmed.to_string())
                }
                None => None,
            };
            if tz.is_some() {
                if c.tz_offset_min != 0 {
                    // tz wins; warn via validation that offset is ignored when tz present
                    // still validate the offset range for storage compat but don't require 0
                    if !(MIN_TZ_OFFSET_MIN..=MAX_TZ_OFFSET_MIN).contains(&c.tz_offset_min) {
                        return Err(format!("params.cron.tz_offset_min must be between {MIN_TZ_OFFSET_MIN} and {MAX_TZ_OFFSET_MIN}"));
                    }
                }
                validate_cron_expr_tz(&c.expr, tz.as_deref(), 0)?;
                Ok(Trigger::Cron {
                    expr: c.expr.clone(),
                    tz_offset_min: c.tz_offset_min,
                    tz,
                })
            } else {
                if !(MIN_TZ_OFFSET_MIN..=MAX_TZ_OFFSET_MIN).contains(&c.tz_offset_min) {
                    return Err(format!("params.cron.tz_offset_min must be between {MIN_TZ_OFFSET_MIN} and {MAX_TZ_OFFSET_MIN}"));
                }
                validate_cron_expr_tz(&c.expr, None, c.tz_offset_min)?;
                Ok(Trigger::Cron {
                    expr: c.expr.clone(),
                    tz_offset_min: c.tz_offset_min,
                    tz: None,
                })
            }
        }
    }
}

fn parse_fire(p: &SetParams) -> Result<Fire, String> {
    let event = p.event.as_ref();
    let action = p.action.as_ref();
    match (event, action) {
        (Some(_), Some(_)) => {
            Err("schedule_set requires exactly one of params.event or params.action".to_string())
        }
        (None, None) => {
            Err("schedule_set requires exactly one of params.event or params.action".to_string())
        }
        (Some(e), None) => {
            check_json_size("event.payload", &e.payload, MAX_JSON_BYTES)?;
            Ok(Fire::Event {
                payload: e.payload.clone(),
            })
        }
        (None, Some(a)) => {
            if a.name.trim().is_empty() {
                return Err("params.action.name must be non-empty".to_string());
            }
            check_size("action.name", &a.name, MAX_ACTION_NAME_BYTES)?;
            check_json_size("action.params", &a.params, MAX_JSON_BYTES)?;
            Ok(Fire::Action {
                name: a.name.trim().to_string(),
                params: a.params.clone(),
            })
        }
    }
}

pub fn parse_request(
    action: &str,
    params_json: &[u8],
    now_ms: i64,
) -> Result<SchedulerRequest, String> {
    match action {
        "schedule_set" => {
            let p: SetParams = serde_json::from_slice(params_json).map_err(|e| {
                format!(
                    "invalid params for schedule_set, expected \
                     {{id?, name?, enabled?, once|cron, event|action}}: {e}"
                )
            })?;
            if let Some(id) = &p.id {
                require_nonempty_id(id.clone())?;
                validate_client_id(id)?;
            }
            let name = match &p.name {
                Some(n) => {
                    let n = n.trim();
                    if n.is_empty() {
                        return Err("params.name must be non-empty when present".to_string());
                    }
                    check_size("name", n, MAX_NAME_BYTES)?;
                    Some(n.to_string())
                }
                None => None,
            };
            let trigger = parse_trigger(&p, now_ms)?;
            let fire = parse_fire(&p)?;
            Ok(SchedulerRequest::Set(NewSchedule {
                id: p.id,
                name,
                enabled: p.enabled.unwrap_or(true),
                trigger,
                fire,
            }))
        }
        "schedule_get" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for schedule_get, expected {{id}}: {e}"))?;
            Ok(SchedulerRequest::Get {
                id: require_nonempty_id(p.id)?,
            })
        }
        "schedule_list" => {
            let p: ListParams = serde_json::from_slice(params_json).map_err(|e| {
                format!("invalid params for schedule_list, expected {{limit?, offset?}}: {e}")
            })?;
            let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
            if limit == 0 || limit > MAX_LIMIT {
                return Err(format!("params.limit must be between 1 and {MAX_LIMIT}"));
            }
            Ok(SchedulerRequest::List {
                limit,
                offset: p.offset.unwrap_or(0),
            })
        }
        "schedule_delete" => {
            let p: IdParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for schedule_delete, expected {{id}}: {e}"))?;
            Ok(SchedulerRequest::Delete {
                id: require_nonempty_id(p.id)?,
            })
        }
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(action: &str, v: serde_json::Value) -> Result<SchedulerRequest, String> {
        parse_request(action, &serde_json::to_vec(&v).unwrap(), 1_000_000)
    }

    fn set(v: serde_json::Value) -> NewSchedule {
        match parse("schedule_set", v).unwrap() {
            SchedulerRequest::Set(s) => s,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_once_at_and_resolves_delay() {
        let s = set(json!({"once": {"at_ms": 5_000}, "event": {"payload": {"k": 1}}}));
        assert_eq!(s.trigger, Trigger::Once { at_ms: 5_000 });
        assert_eq!(
            s.fire,
            Fire::Event {
                payload: json!({"k": 1})
            }
        );
        assert!(s.enabled);

        let s = set(json!({"once": {"delay_ms": 250}, "action": {"name": "notify_send"}}));
        assert_eq!(s.trigger, Trigger::Once { at_ms: 1_000_250 });
        assert_eq!(
            s.fire,
            Fire::Action {
                name: "notify_send".into(),
                params: json!(null)
            }
        );
    }

    #[test]
    fn parses_cron_with_offset_and_name() {
        let s = set(json!({
            "id": "backup-db",
            "name": " nightly ",
            "cron": {"expr": "0 3 * * *", "tz_offset_min": 180},
            "event": {}
        }));
        assert_eq!(s.id.as_deref(), Some("backup-db"));
        assert_eq!(s.name.as_deref(), Some("nightly"));
        assert_eq!(
            s.trigger,
            Trigger::Cron {
                expr: "0 3 * * *".into(),
                tz_offset_min: 180,
                tz: None
            }
        );
    }

    #[test]
    fn rejects_ambiguous_or_missing_trigger_and_fire() {
        let err = parse(
            "schedule_set",
            json!({"once": {"at_ms": 5}, "cron": {"expr": "* * * * *"}, "event": {}}),
        )
        .unwrap_err();
        assert!(
            err.contains("exactly one of params.once or params.cron"),
            "{err}"
        );

        let err = parse("schedule_set", json!({"event": {}})).unwrap_err();
        assert!(
            err.contains("exactly one of params.once or params.cron"),
            "{err}"
        );

        let err = parse("schedule_set", json!({"once": {"at_ms": 5}})).unwrap_err();
        assert!(
            err.contains("exactly one of params.event or params.action"),
            "{err}"
        );

        let err = parse(
            "schedule_set",
            json!({"once": {"at_ms": 5}, "event": {}, "action": {"name": "x"}}),
        )
        .unwrap_err();
        assert!(
            err.contains("exactly one of params.event or params.action"),
            "{err}"
        );
    }

    #[test]
    fn rejects_bad_once_values() {
        let err = parse("schedule_set", json!({"once": {}, "event": {}})).unwrap_err();
        assert!(err.contains("at_ms or delay_ms"), "{err}");

        let err = parse(
            "schedule_set",
            json!({"once": {"at_ms": 5, "delay_ms": 5}, "event": {}}),
        )
        .unwrap_err();
        assert!(err.contains("at_ms or delay_ms"), "{err}");

        let err = parse("schedule_set", json!({"once": {"at_ms": 0}, "event": {}})).unwrap_err();
        assert!(err.contains("positive unix-milliseconds"), "{err}");

        let err = parse(
            "schedule_set",
            json!({"once": {"delay_ms": -1}, "event": {}}),
        )
        .unwrap_err();
        assert!(err.contains(">= 0"), "{err}");

        let err = parse(
            "schedule_set",
            json!({"once": {"delay_ms": MAX_DELAY_MS + 1}, "event": {}}),
        )
        .unwrap_err();
        assert!(err.contains("cap"), "{err}");
    }

    #[test]
    fn rejects_bad_cron_values() {
        let err = parse(
            "schedule_set",
            json!({"cron": {"expr": "definitely not cron"}, "event": {}}),
        )
        .unwrap_err();
        assert!(err.contains("invalid cron expression"), "{err}");

        let err = parse(
            "schedule_set",
            json!({"cron": {"expr": "* * * * *", "tz_offset_min": 9999}, "event": {}}),
        )
        .unwrap_err();
        assert!(err.contains("tz_offset_min"), "{err}");
    }

    #[test]
    fn rejects_bad_ids_names_and_sizes() {
        let err = parse(
            "schedule_set",
            json!({"id": "bad id!", "once": {"at_ms": 5}, "event": {}}),
        )
        .unwrap_err();
        assert!(err.contains("'_' and '-'"), "{err}");

        let err = parse(
            "schedule_set",
            json!({"name": "  ", "once": {"at_ms": 5}, "event": {}}),
        )
        .unwrap_err();
        assert!(err.contains("non-empty"), "{err}");

        let big_payload = Value::String("x".repeat(MAX_JSON_BYTES + 1));
        let err = parse(
            "schedule_set",
            json!({"once": {"at_ms": 5}, "event": {"payload": big_payload}}),
        )
        .unwrap_err();
        assert!(err.contains("serializes to more than"), "{err}");

        let err = parse(
            "schedule_set",
            json!({"once": {"at_ms": 5}, "action": {"name": ""}}),
        )
        .unwrap_err();
        assert!(err.contains("non-empty"), "{err}");

        for action in ["schedule_get", "schedule_delete"] {
            let err = parse(action, json!({"id": ""})).unwrap_err();
            assert!(err.contains("non-empty"), "{err}");
        }

        let err = parse("schedule_list", json!({"limit": 501})).unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn list_defaults_and_unknown_action() {
        match parse("schedule_list", json!({})).unwrap() {
            SchedulerRequest::List { limit, offset } => {
                assert_eq!(limit, DEFAULT_LIMIT);
                assert_eq!(offset, 0);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let err = parse("schedule_frobnicate", json!({})).unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }
}
