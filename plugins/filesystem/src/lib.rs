//! `filesystem` plugin library crate — sandboxed local file browse/read/write.
//!
//! The [`ConcurrentHandler`] implementation lives here (not in the binary
//! crate) because of the orphan rule: the trait comes from `vynkor-sdk` and
//! [`Handler`] from this crate. This is a hot-path plugin with no outbound
//! IPC, so it drives the SDK's concurrent message loop (see
//! `docs/PLUGIN_AUTHORING.md`).

pub mod config;
pub mod handler;
pub mod request;
pub mod sandbox;

use handler::Handler;
use vynkor_sdk::concurrent::response_envelope;
use vynkor_sdk::proto::{ActionRequest, Envelope, PluginManifest};
use vynkor_sdk::ConcurrentHandler;

impl ConcurrentHandler for Handler {
    fn id(&self) -> &str {
        "filesystem"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            permissions: vec![
                "PERMISSION_FILES_READ".into(),
                "PERMISSION_FILES_WRITE".into(),
            ],
            actions: vec!["fs_list".into(), "fs_read".into(), "fs_write".into(), "fs_delete".into(), "fs_mkdir".into(), "fs_rename".into(), "fs_move".into()],
            action_specs: vynkor_plugin_manifest::action_specs(),
            ..Default::default()
        }
    }

    async fn on_action(&self, req: ActionRequest) -> Vec<Envelope> {
        let result = self
            .handle(&req.action, &req.params_json)
            .and_then(|value| {
                serde_json::to_vec(&value)
                    .map_err(|e| format!("ERR_FILES_IO: failed to encode response: {e}"))
            });
        vec![response_envelope(req.action_id, result)]
    }
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
