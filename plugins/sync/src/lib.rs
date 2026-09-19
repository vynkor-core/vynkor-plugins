//! `sync` plugin library crate.
//!
//! The [`ConcurrentHandler`] implementation for [`SyncHandler`] lives here
//! (not in the binary crate) because of the orphan rule: the trait comes
//! from `vynkor-sdk` and the type from this crate, so the impl must be
//! written where the type is defined. It wires the SDK's concurrent message
//! loop to this plugin's request dispatcher, and turns each mutation delta
//! into a best-effort `sync.delta` event publish sent only after the
//! response.

pub mod db;
pub mod handler;
pub mod request;

use vynkor_sdk::concurrent::response_envelope;
use vynkor_sdk::proto::{envelope, ActionRequest, Envelope, EventPublish, PluginManifest};
use vynkor_sdk::ConcurrentHandler;

use handler::{Delta, SyncHandler};

/// Event type this plugin publishes on every mutation. The kernel prepends
/// `plugin.<sender_id>.` at delivery, so subscribers must watch
/// `plugin.sync.sync.delta`.
const DELTA_EVENT_TYPE: &str = "sync.delta";

impl ConcurrentHandler for SyncHandler {
    fn id(&self) -> &str {
        "sync"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            permissions: vec![
                "PERMISSION_STORAGE".into(),
                "PERMISSION_EVENT_PUBLISH".into(),
            ],
            actions: vec![
                "sync_get_snapshot".into(),
                "sync_get".into(),
                "sync_set".into(),
                "sync_del".into(),
            ],
            action_specs: vynkor_plugin_manifest::action_specs(),
            events: vec![DELTA_EVENT_TYPE.into()],
            ..Default::default()
        }
    }

    async fn on_action(&self, req: ActionRequest) -> Vec<Envelope> {
        let mut envelopes = Vec::new();
        match self
            .handle(&req.caller_plugin_id, &req.action, &req.params_json)
            .await
        {
            Ok((response_json, deltas)) => {
                // Response first — the caller's reply never waits on the
                // event publishes that follow.
                envelopes.push(response_envelope(req.action_id, Ok(response_json)));
                // Deltas are already ordered ascending by version (prune
                // deltas before the mutation's own delta).
                envelopes.extend(deltas.into_iter().map(delta_envelope));
            }
            Err(error) => {
                envelopes.push(response_envelope(req.action_id, Err(error)));
            }
        }
        envelopes
    }
}

fn delta_envelope(delta: Delta) -> Envelope {
    Envelope {
        payload: Some(envelope::Payload::EventPublish(EventPublish {
            event_type: DELTA_EVENT_TYPE.to_string(),
            payload_json: delta.payload_json(),
        })),
        ..Default::default()
    }
}

#[cfg(test)]
mod manifest_specs_tests {
    use serde_json::Value;

    // Regression guard: an action that reaches the model with an empty
    // description gets dropped by the agent's embedding filter, and one
    // with no risk label falls through to the agent's name-shaped
    // inference instead of this plugin's own judgement.
    //
    // Reads the shipped plugin.json directly, not action_specs(): that
    // resolves the manifest via current_exe(), which in a dev-build test
    // binary finds nothing and returns an empty list — it cannot tell
    // correct wiring from no wiring at all.
    #[test]
    fn shipped_manifest_documents_and_risk_rates_every_declared_action() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        assert_eq!(
            specs.len(),
            parsed["actions"].as_array().unwrap().len(),
            "every declared action must produce a spec"
        );
        let undocumented: Vec<&str> =
            specs.iter().filter(|s| s.description.is_empty()).map(|s| s.name.as_str()).collect();
        assert!(undocumented.is_empty(), "actions missing a description: {undocumented:?}");
        let unrisked: Vec<&str> = specs
            .iter()
            .filter(|s| s.risk == vynkor_sdk::proto::ActionRisk::Unknown as i32)
            .map(|s| s.name.as_str())
            .collect();
        assert!(unrisked.is_empty(), "actions missing a risk label: {unrisked:?}");
    }
}
