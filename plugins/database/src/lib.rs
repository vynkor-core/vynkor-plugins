//! `database` plugin library crate.
//!
//! The [`ConcurrentHandler`] implementation for [`Handler`] lives here (not
//! in the binary crate) because of the orphan rule: the trait comes from
//! `vynkor-sdk` and the type from this crate, so the impl must be written
//! where the type is defined. It wires the SDK's concurrent message loop to
//! this plugin's request dispatcher.

pub mod db;
pub mod handler;
pub mod request;

use vynkor_sdk::concurrent::response_envelope;
use vynkor_sdk::proto::{envelope, ActionRequest, Envelope, EventPublish, PluginManifest};
use vynkor_sdk::ConcurrentHandler;

use handler::{ChangeEvent, Handler};

pub const PLUGIN_VERSION: &str = "0.2.0";

impl ConcurrentHandler for Handler {
    fn id(&self) -> &str {
        "database"
    }

    fn version(&self) -> &str {
        PLUGIN_VERSION
    }

    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            permissions: vec![
                "PERMISSION_STORAGE".into(),
                "PERMISSION_EVENT_PUBLISH".into(),
            ],
            actions: vec![
                "db_get".into(),
                "db_set".into(),
                "db_delete".into(),
                "db_batch_get".into(),
                "db_query".into(),
                "db_incr".into(),
                "db_keys".into(),
                "db_append".into(),
                "db_patch".into(),
                "status".into(),
            ],
            ..Default::default()
        }
    }

    async fn on_action(&self, req: ActionRequest) -> Vec<Envelope> {
        if req.action == "status" {
            return vec![response_envelope(req.action_id, Ok(self.status_payload()))];
        }
        let mut envelopes = Vec::new();
        match self
            .handle_with_events(&req.caller_plugin_id, &req.action, &req.params_json)
            .await
        {
            Ok((result, change)) => {
                // Response first: the caller's reply never waits on the
                // best-effort event publish that follows.
                envelopes.push(response_envelope(req.action_id, Ok(result)));
                if let Some(change) = change {
                    envelopes.push(change_event_envelope(&req.caller_plugin_id, &change));
                }
            }
            Err(error) => {
                envelopes.push(response_envelope(req.action_id, Err(error)));
            }
        }
        envelopes
    }
}

/// Best-effort `plugin.database.changed` event (the kernel prepends the
/// `plugin.<sender_id>.` namespace). Fire-and-forget, same pattern as
/// network's `request_completed`: it never blocks or alters the caller's
/// response — `on_action` sends the `ActionResponse` envelope first.
fn change_event_envelope(caller: &str, change: &ChangeEvent) -> Envelope {
    let payload_json = serde_json::json!({
        "caller": caller,
        "action": change.action,
        "key": change.key,
    })
    .to_string()
    .into_bytes();
    Envelope {
        payload: Some(envelope::Payload::EventPublish(EventPublish {
            event_type: "changed".into(),
            payload_json,
        })),
        ..Default::default()
    }
}

