//! Thin re-export of the shared `plugin.json` -> `ActionSpec` conversion.
//! See `plugins/_shared/plugin-manifest` (crate `vynkor-plugin-manifest`)
//! for `specs_from_manifest` and its generic unit tests.

pub use vynkor_plugin_manifest::{action_specs, specs_from_manifest};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // Regression guard: an action that reaches the model with an empty
    // description gets dropped by the agent's embedding filter. This test
    // reads this plugin's own shipped plugin.json, so it stays here rather
    // than moving into the generic shared crate.
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
}
