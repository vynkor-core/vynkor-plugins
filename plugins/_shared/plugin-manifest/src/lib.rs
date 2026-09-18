//! Ship the on-disk `plugin.json` action docs to the kernel as
//! `action_specs` on registration.
//!
//! Without this the kernel's `get_manifest` returns an empty `action_specs`
//! array, which silently disables the agent plugin's kernel-manifest
//! catalog layer (`plugins/agent/README.md`, layer 2) and forces every tool
//! description into a hand-maintained operator file that has no link back
//! to the plugin that owns the action — the exact source of the catalog
//! drift that shipped eight references to actions a plugin never had.
//!
//! Deliberately free of MTProto, telegram, and kernel-daemon dependencies
//! so it unit-tests offline. Originally lived in
//! `plugins/telegram/src/manifest.rs`; lifted into this crate so every
//! plugin can depend on it via a local path dependency instead of
//! duplicating it. Candidate for upstreaming into `vynkor-sdk` unchanged
//! once a release cycle there allows it.

use serde_json::Value;
use vynkor_sdk::proto::{ActionRisk, ActionSpec};

fn risk_from_str(raw: &str) -> ActionRisk {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => ActionRisk::Low,
        "medium" => ActionRisk::Medium,
        "high" => ActionRisk::High,
        "critical" => ActionRisk::Critical,
        _ => ActionRisk::Unknown,
    }
}

/// Convert a parsed `plugin.json` into wire `ActionSpec`s. Entries without a
/// `name` are skipped; every other field is optional and degrades to the
/// proto default, so an under-documented manifest still registers.
pub fn specs_from_manifest(manifest: &Value) -> Vec<ActionSpec> {
    let Some(actions) = manifest.get("actions").and_then(Value::as_array) else {
        return Vec::new();
    };
    actions
        .iter()
        .filter_map(|action| {
            let name = action.get("name").and_then(Value::as_str)?;
            Some(ActionSpec {
                name: name.to_string(),
                description: action
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                // The kernel carries params_schema as a JSON-encoded string
                // and does not parse it (see the proto's D-01 note).
                params_schema: action.get("input").map(Value::to_string).unwrap_or_default(),
                risk: risk_from_str(action.get("risk").and_then(Value::as_str).unwrap_or_default())
                    as i32,
                requires_confirmation: action
                    .get("requires_confirmation")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Load `plugin.json` from the installed plugin directory (it sits next to
/// this binary — the manifest's own `files` list installs it there). Any
/// failure degrades to an empty spec list: registration must never depend
/// on the doc file being readable.
pub fn action_specs() -> Vec<ActionSpec> {
    let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("plugin.json")))
    else {
        tracing::warn!("current_exe unavailable; registering without action_specs");
        return Vec::new();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            tracing::warn!(
                path = %path.display(), %err,
                "plugin.json unreadable; registering without action_specs"
            );
            return Vec::new();
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(parsed) => specs_from_manifest(&parsed),
        Err(err) => {
            tracing::warn!(
                path = %path.display(), %err,
                "plugin.json malformed; registering without action_specs"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spec_carries_description_schema_risk_and_confirmation() {
        let manifest = json!({
            "actions": [{
                "name": "tg_send_message",
                "description": "Send a message to a peer",
                "input": {"type": "object"},
                "risk": "medium",
                "requires_confirmation": true
            }]
        });
        let specs = specs_from_manifest(&manifest);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "tg_send_message");
        assert_eq!(specs[0].description, "Send a message to a peer");
        assert_eq!(specs[0].params_schema, r#"{"type":"object"}"#);
        assert_eq!(specs[0].risk, ActionRisk::Medium as i32);
        assert!(specs[0].requires_confirmation);
    }

    #[test]
    fn nameless_entries_are_skipped_and_optional_fields_default() {
        let manifest = json!({"actions": [{"description": "orphan"}, {"name": "tg_ping"}]});
        let specs = specs_from_manifest(&manifest);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "tg_ping");
        assert_eq!(specs[0].description, "");
        assert_eq!(specs[0].params_schema, "");
        assert_eq!(specs[0].risk, ActionRisk::Unknown as i32);
        assert!(!specs[0].requires_confirmation);
    }

    #[test]
    fn unknown_risk_strings_degrade_to_unknown() {
        let manifest = json!({"actions": [{"name": "a", "risk": "SPICY"}]});
        assert_eq!(specs_from_manifest(&manifest)[0].risk, ActionRisk::Unknown as i32);
    }

    #[test]
    fn risk_parsing_is_case_insensitive() {
        let manifest = json!({"actions": [
            {"name": "a", "risk": "LOW"},
            {"name": "b", "risk": " Critical "}
        ]});
        let specs = specs_from_manifest(&manifest);
        assert_eq!(specs[0].risk, ActionRisk::Low as i32);
        assert_eq!(specs[1].risk, ActionRisk::Critical as i32);
    }

    #[test]
    fn manifest_without_actions_yields_no_specs() {
        assert!(specs_from_manifest(&json!({})).is_empty());
        assert!(specs_from_manifest(&json!({"actions": "not an array"})).is_empty());
    }
}
