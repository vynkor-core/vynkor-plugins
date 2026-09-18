//! Thin re-export of the shared `plugin.json` -> `ActionSpec` conversion.
//!
//! The conversion logic used to live here; it was deliberately written
//! free of MTProto and kernel dependencies so it unit-tests offline, and
//! has now been lifted into `plugins/_shared/plugin-manifest` (crate
//! `vynkor-plugin-manifest`) so other plugins can depend on it via a local
//! path dependency instead of duplicating it. See that crate for
//! `specs_from_manifest` and its generic unit tests.
//!
//! `main.rs` keeps calling `telegram_plugin::manifest::action_specs()`, so
//! this module stays as the re-export point rather than pushing the change
//! out to main.rs.

pub use vynkor_plugin_manifest::{action_specs, specs_from_manifest};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // Regression guard for the defect this module exists to fix: an action
    // that reaches the model with an empty description gets dropped by the
    // agent's embedding filter (see the agent plugin's
    // discovery.rs::description_from_parameters doc comment). This test is
    // telegram-specific (it reads telegram's own shipped plugin.json), so
    // it stays here rather than moving into the generic shared crate.
    #[test]
    fn shipped_manifest_documents_every_declared_action() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let declared = parsed["actions"].as_array().unwrap().len();
        let specs = specs_from_manifest(&parsed);
        assert_eq!(specs.len(), declared, "every declared action must produce a spec");
        let undocumented: Vec<&str> = specs
            .iter()
            .filter(|s| s.description.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(undocumented.is_empty(), "actions missing a description: {undocumented:?}");
    }

    #[test]
    fn migrated_actions_ship_a_params_schema() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = specs_from_manifest(&parsed);
        let send = specs.iter().find(|s| s.name == "tg_send_message").unwrap();
        assert!(
            !send.params_schema.is_empty(),
            "tg_send_message must ship its own schema so the operator catalog entry can be deleted"
        );
        let schema: Value = serde_json::from_str(&send.params_schema).unwrap();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("text").is_some());
    }

    // The schemas were lifted from an operator-written tools file that named
    // parameters the handlers never read (`ids`, `from`/`to`, `id` where the
    // handler wants `message_id`). Pin the corrected names against the
    // `params.get(...)` keys in lib.rs so a future edit cannot regress them.
    #[test]
    fn schema_property_names_match_the_handlers() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = specs_from_manifest(&parsed);
        let props_of = |action: &str| -> Value {
            let spec = specs.iter().find(|s| s.name == action).unwrap();
            let schema: Value = serde_json::from_str(&spec.params_schema).unwrap();
            schema["properties"].clone()
        };
        for (action, expected) in [
            ("tg_edit_message", "message_id"),
            ("tg_delete_message", "message_id"),
            ("tg_pin_message", "message_id"),
            ("tg_forward_message", "from_peer"),
        ] {
            let props = props_of(action);
            assert!(
                props.get(expected).is_some(),
                "{action} must document `{expected}` — the handler reads no other key"
            );
        }
        for stale in ["ids", "from", "to", "id"] {
            assert!(
                props_of("tg_forward_message").get(stale).is_none()
                    && props_of("tg_delete_message").get(stale).is_none(),
                "stale operator-file parameter `{stale}` came back"
            );
        }
    }
}
