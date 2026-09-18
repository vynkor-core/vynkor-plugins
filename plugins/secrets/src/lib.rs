//! Library crate for the `secrets` plugin.
//!
//! The `ConcurrentHandler` impl lives here (not in `main`) because of the
//! orphan rule: the trait comes from `vynkor-sdk`, the type from this crate.

pub mod handler;
pub mod request;
pub mod vault;

use std::sync::Arc;

use vynkor_sdk::concurrent::{response_envelope, ConcurrentHandler};
use vynkor_sdk::proto::{ActionRequest, Envelope, PluginManifest};
use vynkor_sdk::VynkorError;

pub const PLUGIN_ID: &str = "secrets";
pub const PLUGIN_VERSION: &str = "0.1.0";

impl ConcurrentHandler for handler::Handler {
    fn id(&self) -> &str {
        PLUGIN_ID
    }

    fn version(&self) -> &str {
        PLUGIN_VERSION
    }

    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            permissions: vec!["PERMISSION_SECRETS".into()],
            actions: vec![
                "secret_set".into(),
                "secret_get".into(),
                "secret_delete".into(),
                "secret_list".into(),
            ],
            action_specs: vynkor_plugin_manifest::action_specs(),
            ..Default::default()
        }
    }

    async fn on_action(&self, req: ActionRequest) -> Vec<Envelope> {
        let result = self
            .handle(&req.caller_plugin_id, &req.action, &req.params_json)
            .await;
        vec![response_envelope(req.action_id, result)]
    }

    async fn on_shutdown(&self) -> Result<(), VynkorError> {
        Ok(())
    }
}

/// Build the plugin handler from environment configuration. Panics on
/// missing/invalid required config (same convention as `database`).
pub fn handler_from_env() -> Arc<handler::Handler> {
    let data_dir = std::env::var("SECRETS_PLUGIN_DATA_DIR").unwrap_or_else(|_| {
        panic!("SECRETS_PLUGIN_DATA_DIR must be set (see config.example.yaml's data_dir)")
    });

    let master_key_raw = std::env::var("SECRETS_PLUGIN_MASTER_KEY").unwrap_or_else(|_| {
        panic!(
            "SECRETS_PLUGIN_MASTER_KEY must be set: 32 bytes as 64 hex chars or 44 base64 chars \
             (generate with: openssl rand -hex 32)"
        )
    });
    let master_key = vault::parse_master_key(&master_key_raw)
        .unwrap_or_else(|e| panic!("SECRETS_PLUGIN_MASTER_KEY invalid: {e}"));

    let max_name_bytes = std::env::var("SECRETS_PLUGIN_MAX_NAME_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(request::DEFAULT_MAX_NAME_BYTES);
    let max_value_bytes = std::env::var("SECRETS_PLUGIN_MAX_VALUE_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(request::DEFAULT_MAX_VALUE_BYTES);

    Arc::new(handler::Handler::new(
        data_dir.into(),
        master_key,
        max_name_bytes,
        max_value_bytes,
    ))
}

#[cfg(test)]
mod manifest_specs_tests {
    use serde_json::Value;

    // Regression guard: an action that reaches the model with an empty
    // description gets dropped by the agent's embedding filter. This reads
    // the shipped plugin.json directly (not action_specs(), which relies on
    // current_exe() and is meaningless in a dev-build test binary).
    #[test]
    fn shipped_manifest_documents_every_declared_action() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let declared = parsed["actions"].as_array().unwrap().len();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        assert_eq!(specs.len(), declared, "every declared action must produce a spec");
        let undocumented: Vec<&str> = specs
            .iter()
            .filter(|s| s.description.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(undocumented.is_empty(), "actions missing a description: {undocumented:?}");
    }
}
