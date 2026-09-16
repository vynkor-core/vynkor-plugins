//! Request parsing/validation for the `automations` rule CRUD actions.
//! House style: every violation names the offending field.

use serde::Deserialize;
use serde_json::Value;

use crate::store::{ActionSpec, Condition, RuleDoc, Trigger};
#[cfg(test)]
use crate::store::MAX_RULES;

pub const MAX_NAME_BYTES: usize = 200;
pub const MAX_CONDITIONS: usize = 8;

#[derive(Debug)]
pub enum AutomationRequest {
    /// Create (no id) or update (id present) one rule.
    RuleSet { doc: RuleDoc },
    RuleGet { id: String },
    RuleList,
    /// Delete; also used to retract a confirmation-pending rule.
    RuleDelete { id: String },
}

#[derive(Deserialize)]
struct SetParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    trigger: Trigger,
    #[serde(default)]
    conditions: Vec<Condition>,
    action: ActionSpec,
    #[serde(default)]
    requires_confirmation: bool,
    #[serde(default)]
    cooldown_ms: u64,
}

fn default_true() -> bool {
    true
}

fn want_id(body: &Value) -> Result<String, String> {
    body.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing required field: id".to_string())
}

/// Validate the caller-supplied parts of a rule. Storage-owned fields
/// (`last_fired_ms`, `last_error`, `fire_count`, timestamps) are ignored in
/// requests and managed by the engine.
fn validate(doc: &RuleDoc) -> Result<(), String> {
    if doc.name.len() > MAX_NAME_BYTES {
        return Err(format!("params.name exceeds {MAX_NAME_BYTES} bytes"));
    }
    let trigger_event = &doc.trigger.event_type;
    if trigger_event.is_empty() || trigger_event.len() > 200 {
        return Err("params.trigger.event_type must be 1..=200 chars".to_string());
    }
    if !trigger_event
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(
            "params.trigger.event_type may only contain [A-Za-z0-9._-]".to_string(),
        );
    }
    if doc.conditions.len() > MAX_CONDITIONS {
        return Err(format!("params.conditions exceeds {MAX_CONDITIONS} entries"));
    }
    for condition in &doc.conditions {
        if !condition.path.starts_with('/') && !condition.path.is_empty() {
            return Err(format!(
                "params.conditions.path {:?} must be a JSON pointer starting with '/'",
                condition.path
            ));
        }
        if condition.equals.is_null() {
            return Err(format!(
                "params.conditions.equals must not be null for path {:?} — null is \
                 indistinguishable from a missing value",
                condition.path
            ));
        }
    }
    if doc.action.target_action.is_empty() || doc.action.target_action.len() > 128 {
        return Err("params.action.target_action must be 1..=128 chars".to_string());
    }
    if serde_json::to_vec(&doc.action.params_json)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
        > 32 * 1024
    {
        return Err("params.action.params_json exceeds 32 KiB".to_string());
    }
    Ok(())
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<AutomationRequest, String> {
    let body: Value = serde_json::from_slice(params_json)
        .map_err(|e| format!("invalid JSON params for {action}: {e}"))?;
    match action {
        "rule_set" => {
            let p: SetParams = serde_json::from_value(body).map_err(|e| {
                format!(
                    "invalid params for rule_set, expected {{id?, name?, enabled?, \
                     trigger{{event_type}}, conditions?[], \
                     action{{target_action, params_json}}, requires_confirmation?, \
                     cooldown_ms?}}: {e}"
                )
            })?;
            let now = crate::store::now_ms();
            let doc = RuleDoc {
                id: p.id.unwrap_or_default(),
                name: p.name.trim().to_string(),
                enabled: p.enabled,
                trigger: p.trigger,
                conditions: p.conditions,
                action: p.action,
                requires_confirmation: p.requires_confirmation,
                cooldown_ms: p.cooldown_ms,
                last_fired_ms: 0,
                last_error: String::new(),
                fire_count: 0,
                created_at_ms: now,
                updated_at_ms: now,
            };
            validate(&doc)?;
            Ok(AutomationRequest::RuleSet { doc })
        }
        "rule_get" => Ok(AutomationRequest::RuleGet { id: want_id(&body)? }),
        "rule_list" => Ok(AutomationRequest::RuleList),
        "rule_delete" => Ok(AutomationRequest::RuleDelete { id: want_id(&body)? }),
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_rule(v: Value) -> Value {
        let mut merged = json!({
            "enabled": true,
            "trigger": {"event_type": "plugin.calendar.due"},
            "action": {"target_action": "notify_send", "params_json": {"message": "m"}}
        });
        if let (Some(a), Some(b)) = (merged.as_object_mut(), v.as_object()) {
            for (k, val) in b {
                a.insert(k.clone(), val.clone());
            }
        }
        merged
    }

    #[test]
    fn parses_minimal_set() {
        match parse_request("rule_set", &serde_json::to_vec(&base_rule(json!({}))).unwrap())
        {
            Ok(AutomationRequest::RuleSet { doc }) => {
                assert!(doc.id.is_empty(), "no id = create");
                assert!(doc.enabled);
                assert!(doc.conditions.is_empty());
                assert_eq!(doc.trigger.event_type, "plugin.calendar.due");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_trigger_and_conditions() {
        let err = parse_request(
            "rule_set",
            &serde_json::to_vec(&base_rule(json!({"trigger": {"event_type": "bad type!"}})))
                .unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("event_type"), "{err}");

        let err = parse_request(
            "rule_set",
            &serde_json::to_vec(&base_rule(json!({
                "conditions": [{"path": "nopath", "equals": 1}]
            })))
            .unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("JSON pointer"), "{err}");

        let err = parse_request(
            "rule_set",
            &serde_json::to_vec(&base_rule(json!({
                "conditions": [{"path": "/x", "equals": null}]
            })))
            .unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("must not be null"), "{err}");
    }

    #[test]
    fn get_delete_require_id_and_unknown_actions_rejected() {
        let err =
            parse_request("rule_get", b"{}").unwrap_err();
        assert!(err.contains("id"), "{err}");
        let ok = parse_request("rule_delete", br#"{"id":"7"}"#).unwrap();
        assert!(matches!(ok, AutomationRequest::RuleDelete { id } if id == "7"));
        let err = parse_request("rule_frobnicate", b"{}").unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }

    #[test]
    fn max_rules_cap_is_sane() {
        assert_eq!(MAX_RULES, 200);
    }
}
