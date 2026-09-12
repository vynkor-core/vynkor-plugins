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
