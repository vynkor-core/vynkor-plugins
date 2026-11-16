//! Which device commands may run without the agent's confirmation gate.
//!
//! The gate is static per action, so the plugin exposes two: the low-risk
//! `mqtt_device_command` executes only what this allowlist admits, and the
//! confirmed `mqtt_device_command_confirmed` executes anything after the user
//! says yes.
//!
//! Spec (`MQTT_PLUGIN_COMMAND_ALLOWLIST`): comma-separated rules
//! `device:command[:target]`. `*` matches any device or command. `target`
//! restricts the rule to one pin, matched against `params.name` or
//! `params.pin`; a rule with a target never matches a call that names none,
//! sets both, or sets one to anything but a string or number.
//!
//! ```text
//! uesp12e-001:set_pin:led_d1,uesp12e-001:toggle_pin:led_d1,*:get_state
//! ```

use serde_json::Value;

pub const ENV: &str = "MQTT_PLUGIN_COMMAND_ALLOWLIST";

/// Read-only commands: safe on any device.
pub const DEFAULT: &str = "*:get_state,*:ping";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    device: String,
    command: String,
    target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPolicy {
    rules: Vec<Rule>,
}

impl CommandPolicy {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut rules = Vec::new();
        for raw in spec.split(',').map(str::trim).filter(|r| !r.is_empty()) {
            let parts: Vec<&str> = raw.split(':').map(str::trim).collect();
            let (device, command, target) = match parts.as_slice() {
                [d, c] => (*d, *c, None),
                [d, c, t] => (*d, *c, Some(*t)),
                _ => return Err(format!("rule '{raw}': expected device:command[:target]")),
            };
            if device.is_empty() || command.is_empty() || target == Some("") {
                return Err(format!("rule '{raw}': empty field"));
            }
            if target == Some("*") {
                return Err(format!("rule '{raw}': omit the target instead of '*'"));
            }
            rules.push(Rule {
                device: device.into(),
                command: command.into(),
                target: target.map(String::from),
            });
        }
        Ok(Self { rules })
    }

    /// The env allowlist, or the read-only default when it is unset.
    ///
    /// A malformed spec falls back to the default rather than to "allow
    /// nothing": the plugin stays usable, and the default never actuates.
    pub fn from_env() -> Self {
        let spec = std::env::var(ENV).unwrap_or_else(|_| DEFAULT.into());
        Self::parse(&spec).unwrap_or_else(|error| {
            tracing::error!(%error, "invalid {ENV}; using the read-only default");
            Self::parse(DEFAULT).expect("default allowlist parses")
        })
    }

    pub fn allows(&self, device: &str, command: &str, params: &Value) -> bool {
        let target = Self::target(params);
        self.rules.iter().any(|rule| {
            (rule.device == "*" || rule.device == device)
                && (rule.command == "*" || rule.command == command)
                && match &rule.target {
                    None => true,
                    Some(want) => matches!(&target, Ok(Some(got)) if got == want),
                }
        })
    }
}

impl CommandPolicy {
    /// The single pin a call addresses.
    ///
    /// `Err` when that is ambiguous: both `name` and `pin` set (the rule
    /// would check one while the firmware may act on the other), or either
    /// one set to something other than a string or number. Ambiguity never
    /// matches a target rule.
    fn target(params: &Value) -> Result<Option<String>, ()> {
        let mut found = None;
        for key in ["name", "pin"] {
            let value = match &params[key] {
                Value::Null => continue,
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return Err(()),
            };
            if found.replace(value).is_some() {
                return Err(());
            }
        }
        Ok(found)
    }
}

impl Default for CommandPolicy {
    fn default() -> Self {
        Self::parse(DEFAULT).expect("default allowlist parses")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HOME: &str = "uesp12e-001:set_pin:led_d1, uesp12e-001:toggle_pin:led_d1, *:get_state";

    #[test]
    fn default_admits_only_read_only_commands() {
        let policy = CommandPolicy::default();
        assert!(policy.allows("any", "get_state", &json!({})));
        assert!(policy.allows("any", "ping", &json!({})));
        assert!(!policy.allows("any", "set_pin", &json!({"name": "led_d1", "value": 1})));
        assert!(!policy.allows("any", "ota_update", &json!({})));
    }

    #[test]
    fn target_rule_admits_only_its_pin() {
        let policy = CommandPolicy::parse(HOME).unwrap();
        assert!(policy.allows(
            "uesp12e-001",
            "set_pin",
            &json!({"name": "led_d1", "value": 1})
        ));
        assert!(!policy.allows(
            "uesp12e-001",
            "set_pin",
            &json!({"name": "relay", "value": 1})
        ));
        assert!(!policy.allows("other-device", "set_pin", &json!({"name": "led_d1"})));
        assert!(
            !policy.allows("uesp12e-001", "set_pin", &json!({})),
            "a call that names no pin must not match a pin rule"
        );
    }

    #[test]
    fn numeric_pin_matches_its_string_form() {
        let policy = CommandPolicy::parse("esp:set_relay:5").unwrap();
        assert!(policy.allows("esp", "set_relay", &json!({"pin": 5, "state": true})));
        assert!(!policy.allows("esp", "set_relay", &json!({"pin": 4, "state": true})));
    }

    #[test]
    fn wildcard_command_admits_everything_on_that_device() {
        let policy = CommandPolicy::parse("lab-board:*").unwrap();
        assert!(policy.allows("lab-board", "set_pin", &json!({"name": "x"})));
        assert!(!policy.allows("uesp12e-001", "set_pin", &json!({"name": "x"})));
    }

    #[test]
    fn malformed_rules_are_rejected() {
        for bad in [
            "esp",
            "esp:set_pin:a:b",
            ":set_pin",
            "esp:",
            "esp:set_pin:",
            "esp:set_pin:*",
        ] {
            assert!(CommandPolicy::parse(bad).is_err(), "accepted '{bad}'");
        }
    }

    #[test]
    fn ambiguous_target_never_matches_a_target_rule() {
        let policy = CommandPolicy::parse("esp:set_pin:led_d1").unwrap();
        // The rule sees name=led_d1, the firmware may act on pin=5.
        assert!(!policy.allows("esp", "set_pin", &json!({"name": "led_d1", "pin": 5})));
        assert!(!policy.allows("esp", "set_pin", &json!({"name": ["led_d1"]})));
        assert!(!policy.allows("esp", "set_pin", &json!({"name": {"id": "led_d1"}})));
        assert!(!policy.allows("esp", "set_pin", &json!({"name": "led_d1", "pin": true})));
    }

    #[test]
    fn rule_without_target_ignores_params_shape() {
        let policy = CommandPolicy::parse("esp:get_state").unwrap();
        assert!(policy.allows("esp", "get_state", &json!({"name": "a", "pin": 1})));
    }

    #[test]
    fn empty_spec_allows_nothing() {
        let policy = CommandPolicy::parse("").unwrap();
        assert!(!policy.allows("any", "get_state", &json!({})));
    }
}
