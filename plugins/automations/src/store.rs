//! Typed storage for `automations` rules over the `database` plugin —
//! same thin-schema contract as `notes`/`calendar`: kernel-routed actions
//! only, private namespace stamped by the kernel's `caller_plugin_id`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Counter key backing rule ids (atomic via `db_incr`).
pub const NEXT_ID_KEY: &str = "meta:next_id";
/// Key prefix for rule documents: `rule:<id>` → JSON [`RuleDoc`].
pub const KEY_PREFIX: &str = "rule:";
pub const MAX_RULES: usize = 200;

/// One condition: JSON-pointer path into the event payload plus an exact
/// expected value. All conditions must hold (AND); empty = always true.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// JSON pointer into the event payload, e.g. `/battery_level` (`""` =
    /// whole payload). Pointer segments with `/` inside are not addressable.
    pub path: String,
    pub equals: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    /// Fully-qualified event type as delivered by the kernel, e.g.
    /// `plugin.calendar.due`, `plugin.scheduler.fired`,
    /// `plugin.sync.sync.delta`. Must be in the operator's subscription set.
    pub event_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionSpec {
    /// Kernel-routed action to dispatch when the rule fires.
    pub target_action: String,
    /// Params sent verbatim to the target action.
    #[serde(default)]
    pub params_json: Value,
}

/// One automation rule. Stored under `rule:<id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleDoc {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub enabled: bool,
    pub trigger: Trigger,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    pub action: ActionSpec,
    /// When set, the rule never auto-dispatches; it publishes a
    /// `needs_confirmation` event instead and the operator re-enables it via
    /// `rule_set` after review.
    #[serde(default)]
    pub requires_confirmation: bool,
    /// Minimum ms between fires (`0` = fire on every matching event).
    #[serde(default)]
    pub cooldown_ms: u64,
    #[serde(default)]
    pub last_fired_ms: i64,
    #[serde(default)]
    pub last_error: String,
    #[serde(default)]
    pub fire_count: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl RuleDoc {
    /// All conditions hold against the decoded event payload?
    pub fn conditions_hold(&self, payload: &Value) -> bool {
        self.conditions.iter().all(|c| {
            let actual =
                if c.path.is_empty() { Some(payload) } else { payload.pointer(&c.path) };
            actual.map(|v| v == &c.equals).unwrap_or(false)
        })
    }

    /// Cooldown satisfied relative to `now_ms`?
    pub fn cooldown_ok(&self, now_ms: i64) -> bool {
        self.cooldown_ms == 0
            || now_ms - self.last_fired_ms >= self.cooldown_ms as i64
    }
}

pub struct Db {
    rpc: crate::Rpc,
    timeout_ms: u32,
}

impl Db {
    pub fn new(rpc: crate::Rpc, timeout_ms: u32) -> Self {
        Self { rpc, timeout_ms }
    }

    async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
        let params_json =
            serde_json::to_vec(&params).map_err(|e| format!("failed to encode {action} params: {e}"))?;
        self.rpc.call(action, params_json, self.timeout_ms).await
    }

    pub async fn next_id(&self) -> Result<u64, String> {
        let v = self.call("db_incr", serde_json::json!({"key": NEXT_ID_KEY})).await?;
        v.get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("database.db_incr returned unexpected payload: {v}"))
    }

    pub async fn put(&self, rule: &RuleDoc) -> Result<(), String> {
        let key = format!("{KEY_PREFIX}{}", rule.id);
        let v = self.call("db_set", serde_json::json!({"key": key, "value": rule})).await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("database.db_set returned unexpected payload: {v}"));
        }
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<RuleDoc>, String> {
        let v = self
            .call("db_get", serde_json::json!({"key": format!("{KEY_PREFIX}{id}")}))
            .await?;
        if v.get("found").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        serde_json::from_value(v.get("value").cloned().unwrap_or(Value::Null))
            .map(Some)
            .map_err(|e| format!("stored rule {id:?} is corrupt: {e}"))
    }

    pub async fn list(&self) -> Result<Vec<RuleDoc>, String> {
        let v = self.call("db_keys", serde_json::json!({"prefix": KEY_PREFIX})).await?;
        let keys: Vec<String> = v
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("database.db_keys returned unexpected payload: {v}"))?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let v = self.call("db_batch_get", serde_json::json!({"keys": keys})).await?;
        let values = v
            .get("values")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("database.db_batch_get returned unexpected payload: {v}"))?;
        Ok(values
            .values()
            .filter_map(|value| serde_json::from_value::<RuleDoc>(value.clone()).ok())
            .collect())
    }

    pub async fn delete(&self, id: &str) -> Result<bool, String> {
        let v = self
            .call("db_delete", serde_json::json!({"key": format!("{KEY_PREFIX}{id}")}))
            .await?;
        v.get("deleted")
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("database.db_delete returned unexpected payload: {v}"))
    }
}

/// Current unix time in milliseconds (same saturating pattern as calendar).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule_with(conditions: Vec<Condition>, cooldown_ms: u64, last_fired_ms: i64) -> RuleDoc {
        RuleDoc {
            id: "1".to_string(),
            name: "test".to_string(),
            enabled: true,
            trigger: Trigger { event_type: "plugin.test.fired".to_string() },
            conditions,
            action: ActionSpec {
                target_action: "noop".to_string(),
                params_json: Value::Null,
            },
            requires_confirmation: false,
            cooldown_ms,
            last_fired_ms,
            last_error: String::new(),
            fire_count: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn empty_conditions_always_hold() {
        let rule = rule_with(vec![], 0, 0);
        assert!(rule.conditions_hold(&serde_json::json!({"anything": 1})));
        assert!(rule.conditions_hold(&Value::Null));
    }

    #[test]
    fn empty_path_matches_whole_payload() {
        let payload = serde_json::json!({"a": 1});
        let matching = rule_with(
            vec![Condition { path: String::new(), equals: payload.clone() }],
            0,
            0,
        );
        assert!(matching.conditions_hold(&payload));

        let mismatching = rule_with(
            vec![Condition { path: String::new(), equals: serde_json::json!({"a": 2}) }],
            0,
            0,
        );
        assert!(!mismatching.conditions_hold(&payload));
    }

    #[test]
    fn missing_pointer_path_does_not_hold() {
        let rule = rule_with(
            vec![Condition { path: "/nope".to_string(), equals: serde_json::json!(1) }],
            0,
            0,
        );
        assert!(!rule.conditions_hold(&serde_json::json!({"a": 1})));
    }

    #[test]
    fn type_mismatch_does_not_hold() {
        let rule = rule_with(
            vec![Condition { path: "/a".to_string(), equals: serde_json::json!("1") }],
            0,
            0,
        );
        // payload has a.a == 1 (number), condition expects "1" (string) — no match.
        assert!(!rule.conditions_hold(&serde_json::json!({"a": 1})));
    }

    #[test]
    fn multiple_conditions_are_and_combined() {
        let rule = rule_with(
            vec![
                Condition { path: "/a".to_string(), equals: serde_json::json!(1) },
                Condition { path: "/b".to_string(), equals: serde_json::json!(2) },
            ],
            0,
            0,
        );
        assert!(rule.conditions_hold(&serde_json::json!({"a": 1, "b": 2})));
        assert!(!rule.conditions_hold(&serde_json::json!({"a": 1, "b": 3})));
        assert!(!rule.conditions_hold(&serde_json::json!({"a": 1})));
    }

    #[test]
    fn zero_cooldown_always_ok() {
        let rule = rule_with(vec![], 0, 1_000_000);
        assert!(rule.cooldown_ok(1_000_000));
        assert!(rule.cooldown_ok(0));
    }

    #[test]
    fn cooldown_boundary_is_inclusive() {
        let rule = rule_with(vec![], 10_000, 1_000_000);
        // exactly on the boundary: elapsed == cooldown_ms -> ok.
        assert!(rule.cooldown_ok(1_010_000));
        // one ms short of the boundary -> not ok.
        assert!(!rule.cooldown_ok(1_009_999));
        // well past the boundary -> ok.
        assert!(rule.cooldown_ok(2_000_000));
    }

    #[test]
    fn cooldown_ok_when_never_fired() {
        let rule = rule_with(vec![], 5_000, 0);
        assert!(rule.cooldown_ok(now_ms()));
    }
}
