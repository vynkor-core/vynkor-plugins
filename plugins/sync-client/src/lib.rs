//! `sync-client` plugin — the D-13 client side: a local mirror of the host
//! `sync` plugin's store.
//!
//! On every (re)connect the plugin subscribes to the host's delta events,
//! pulls a `sync_get_snapshot` to seed the mirror, then applies deltas to it
//! as they arrive. A background heartbeat task pushes the device's liveness
//! into the host's sync state via `sync_set`, and reconnect re-pulls the
//! snapshot so an offline device catches up (authoritative reset — stale
//! keys dropped while offline disappear).
//!
//! The mirror is `Arc<RwLock<Mirror>>` because the SDK's concurrent loop
//! invokes handlers through `&self` from multiple spawned tasks; interior
//! mutation is the only way to share state without locking the client.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::RwLock;

use vynkor_sdk::concurrent::{response_envelope, ConcurrentHandler};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionStatus, Envelope, Event, PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

pub mod request;

/// Local mirror of the host sync store. `version` is the store version the
/// mirror is caught up to; deltas at or below it are stale and skipped.
#[derive(Default, Debug, serde::Serialize)]
pub struct Mirror {
    pub version: u64,
    pub state: HashMap<String, serde_json::Value>,
}

/// One delta event from the host sync plugin (`plugin.sync.sync.delta`).
/// `version` is the store version AFTER the mutation; `value` is null for
/// `op: "del"`.
#[derive(serde::Deserialize)]
struct Delta {
    op: String,
    key: String,
    #[serde(default)]
    value: serde_json::Value,
    version: u64,
}

/// `sync_get_snapshot` result payload.
#[derive(serde::Deserialize)]
struct Snapshot {
    version: u64,
    state: HashMap<String, serde_json::Value>,
}

/// Handler for the sync-client plugin. Shared behind an `Arc` so the serve
/// loop, the heartbeat task and every spawned action task can reach the
/// mirror through `&self`.
pub struct SyncClientHandler {
    mirror: Arc<RwLock<Mirror>>,
    device_id: String,
    snapshot_timeout_ms: u32,
}

impl SyncClientHandler {
    pub fn new(device_id: String, snapshot_timeout_ms: u32) -> Self {
        Self {
            mirror: Arc::new(RwLock::new(Mirror::default())),
            device_id,
            snapshot_timeout_ms,
        }
    }

    async fn handle_action(&self, action: &str, params_json: &[u8]) -> Result<Vec<u8>, String> {
        let _req = request::parse_request(action, params_json)?;
        let mirror = self.mirror.read().await;
        serde_json::to_vec(&*mirror).map_err(|e| format!("failed to encode response: {e}"))
    }
}

impl ConcurrentHandler for SyncClientHandler {
    fn id(&self) -> &str {
        "sync-client"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            permissions: vec!["PERMISSION_SCHEDULER".into(), "PERMISSION_IPC_SEND".into()],
            actions: vec!["sync_client_get_state".into()],
            action_specs: vynkor_plugin_manifest::action_specs(),
            events: vec!["plugin.sync.sync.delta".into()],
            // caller side of the shared contract: this plugin invokes the
            // `sync` plugin's actions, so `sync` must be in the ipc_targets
            // allowlist (kernel default-deny peer IPC, T-04).
            ipc_targets: vec!["sync".into()],
            ..Default::default()
        }
    }

    async fn on_init(&self, client: &mut VynkorClient) -> Result<(), VynkorError> {
        client
            .subscribe(vec!["plugin.sync.sync.delta".to_string()])
            .await?;
        // Authoritative reset: replace the whole mirror with the snapshot so
        // keys deleted while we were offline disappear. Best-effort — on any
        // failure keep the stale mirror rather than wiping local state.
        match client
            .send_action("sync_get_snapshot", b"{}", self.snapshot_timeout_ms)
            .await
        {
            Ok(resp) if resp.status == ActionStatus::ActionOk as i32 => {
                match serde_json::from_slice::<Snapshot>(&resp.data_json) {
                    Ok(snapshot) => {
                        *self.mirror.write().await = Mirror {
                            version: snapshot.version,
                            state: snapshot.state,
                        };
                    }
                    Err(e) => eprintln!("[sync-client] bad snapshot payload: {e}"),
                }
            }
            Ok(resp) => eprintln!(
                "[sync-client] snapshot returned status {} ({}); keeping stale mirror",
                resp.status, resp.error
            ),
            Err(e) => eprintln!("[sync-client] snapshot pull failed: {e}"),
        }
        Ok(())
    }

    async fn on_event(&self, event: Event) -> Result<Option<Envelope>, VynkorError> {
        if event.event_type != "plugin.sync.sync.delta" {
            return Ok(None);
        }
        let delta: Delta = match serde_json::from_slice(&event.payload_json) {
            Ok(d) => d,
            Err(e) => {
                // a malformed delta can't be fixed by redelivery — ack it so
                // the kernel stops retrying.
                eprintln!("[sync-client] bad delta payload: {e}");
                return Ok(None);
            }
        };
        let mut mirror = self.mirror.write().await;
        // version is post-mutation; at-or-below means the snapshot or an
        // earlier delta already covered it (in-order per-connection delivery
        // makes this correct).
        if delta.version <= mirror.version {
            return Ok(None);
        }
        match delta.op.as_str() {
            "set" => {
                mirror.state.insert(delta.key, delta.value);
            }
            "del" => {
                mirror.state.remove(&delta.key);
            }
            other => eprintln!("[sync-client] unknown delta op: {other}"),
        }
        mirror.version = delta.version;
        Ok(None)
    }

    async fn on_action(&self, req: ActionRequest) -> Vec<Envelope> {
        let result = self.handle_action(&req.action, &req.params_json).await;
        vec![response_envelope(req.action_id, result)]
    }

    async fn on_message(&self, _env: Envelope) -> Result<Option<Envelope>, VynkorError> {
        // heartbeat `sync_set` ActionResponses land here; fire-and-forget, so
        // nothing to do.
        Ok(None)
    }
}

/// Drive the sync-client message loop: one task owns the client and
/// `select!`s between inbound frames and the mpsc channel that both spawned
/// handler tasks and the heartbeat task push into. Copied from
/// `vynkor-sdk/src/concurrent.rs::run_concurrent_loop` with one difference —
/// the heartbeat producer task — which is why the SDK's loop can't be used
/// as-is (it doesn't expose its channel).
pub async fn run_sync_client_loop(
    mut client: VynkorClient,
    handler: Arc<SyncClientHandler>,
    heartbeat_secs: u64,
) -> Result<(), VynkorError> {
    let (tx, mut rx) = mpsc::channel::<Envelope>(64);

    if heartbeat_secs > 0 {
        tokio::spawn(heartbeat_task(handler.clone(), tx.clone(), heartbeat_secs));
    }

    loop {
        tokio::select! {
            envelope = client.recv() => {
                let envelope = match envelope {
                    Ok(env) => env,
                    Err(_) => break, // disconnect / EOF
                };

                match envelope.payload {
                    Some(envelope::Payload::Ping(ping)) => {
                        let pong = Envelope {
                            payload: Some(envelope::Payload::Pong(Pong {
                                original_timestamp: ping.timestamp,
                                server_timestamp: unix_millis(),
                            })),
                            ..Default::default()
                        };
                        let _ = client.send("kernel", pong).await;
                    }
                    Some(envelope::Payload::PluginShutdown(_)) => break,
                    Some(envelope::Payload::ActionRequest(req)) => {
                        match handler.accept(&req) {
                            Ok(()) => spawn_handler(handler.clone(), tx.clone(), req),
                            Err(error) => {
                                let envelope =
                                    response_envelope(req.action_id.clone(), Err(error));
                                let _ = client.send("kernel", envelope).await;
                            }
                        }
                    }
                    Some(envelope::Payload::Event(event)) => {
                        let event_id = event.event_id.clone();
                        // on handler error no ack is sent — the kernel will retry.
                        if let Ok(reply) = handler.on_event(event).await {
                            let _ = client.ack_event(&event_id).await;
                            if let Some(resp) = reply {
                                let _ = client.send("kernel", resp).await;
                            }
                        }
                    }
                    Some(other) => {
                        if let Ok(Some(reply)) = handler.on_message(Envelope {
                            payload: Some(other),
                            ..Default::default()
                        }).await {
                            let _ = client.send("kernel", reply).await;
                        }
                    }
                    None => {}
                }
            }
            Some(response_envelope) = rx.recv() => {
                let _ = client.send("kernel", response_envelope).await;
            }
        }
    }

    Ok(())
}

/// Register `handler` with the kernel, run [`SyncClientHandler::on_init`]
/// (subscribe + snapshot pull), then drive the custom loop. Mirrors the SDK's
/// `serve_concurrent` but with [`run_sync_client_loop`] in place of its
/// generic loop.
pub async fn serve_cycle(
    mut client: VynkorClient,
    jwt_token: &str,
    handler: Arc<SyncClientHandler>,
    heartbeat_secs: u64,
) -> Result<(), VynkorError> {
    let ack = client
        .register_full(
            handler.id(),
            handler.version(),
            handler.manifest(),
            jwt_token,
        )
        .await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!(
            "registration rejected: {}",
            ack.reject_reason
        )));
    }
    if let Err(e) = handler.on_init(&mut client).await {
        let _ = handler.on_shutdown().await;
        return Err(e);
    }
    let result = run_sync_client_loop(client, handler.clone(), heartbeat_secs).await;
    let _ = handler.on_shutdown().await;
    result
}

/// Fire a `sync_set` ActionRequest every `heartbeat_secs` into the loop's
/// channel. `sync_set` (not `publish_event`) is the shared contract: the host
/// sync plugin folds the heartbeat value into its store and re-broadcasts it
/// as a delta. Exits when the receiver drops (the loop ended).
async fn heartbeat_task(
    handler: Arc<SyncClientHandler>,
    tx: mpsc::Sender<Envelope>,
    heartbeat_secs: u64,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(heartbeat_secs));
    let mut seq: u64 = 0;
    loop {
        interval.tick().await;
        seq += 1;

        let (device_id, version) = {
            let mirror = handler.mirror.read().await;
            (handler.device_id.clone(), mirror.version)
        };
        let key = format!("heartbeat.{device_id}");
        let value = serde_json::json!({
            "ts": unix_millis(),
            "device_id": device_id,
            "version": version,
        });
        let params = serde_json::json!({ "key": key, "value": value });
        let params_json = match serde_json::to_vec(&params) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[sync-client] heartbeat params encode failed: {e}");
                continue;
            }
        };
        let envelope = Envelope {
            payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                action_id: format!("hb-{seq}"),
                action: "sync_set".into(),
                params_json,
                timeout_ms: 0,
                streaming: false,
                caller_plugin_id: "sync-client".into(),
            })),
            ..Default::default()
        };
        if tx.send(envelope).await.is_err() {
            break; // loop gone; stop beating
        }
    }
}

/// Spawn a handler task that always produces at least one response envelope
/// on `tx`, even if [`ConcurrentHandler::on_action`] panics. Double-spawns so
/// the outer task can always reach `tx.send` (a panic surfaces as
/// `Err(JoinError)`, converted to an `ACTION_ERROR` response).
fn spawn_handler<H: ConcurrentHandler>(
    handler: Arc<H>,
    tx: mpsc::Sender<Envelope>,
    req: ActionRequest,
) {
    tokio::spawn(async move {
        let inner = handler.clone();
        let action_id = req.action_id.clone();
        let join = tokio::spawn(async move { inner.on_action(req).await });
        let envelopes = match join.await {
            Ok(envelopes) => envelopes,
            Err(join_err) => {
                vec![response_envelope(
                    action_id,
                    Err(format!("handler panicked: {join_err}")),
                )]
            }
        };
        for envelope in envelopes {
            let _ = tx.send(envelope).await;
        }
    });
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use vynkor_sdk::proto::{ActionResponse, PluginRegisterAck, PluginShutdown, Subscribe};

    /// Recv the next frame from the plugin with a timeout so a deadlock turns
    /// into a test failure instead of a CI hang (SDK `UnixStream::pair`
    /// pattern).
    async fn recv(kernel: &mut VynkorClient) -> Envelope {
        tokio::time::timeout(Duration::from_secs(5), kernel.recv())
            .await
            .expect("timed out waiting for plugin frame")
            .expect("plugin stream closed unexpectedly")
    }

    /// Drive the register → subscribe → snapshot handshake the plugin's
    /// `serve_cycle` performs, answering the snapshot pull with `snapshot`.
    async fn drive_registration(kernel: &mut VynkorClient, snapshot: serde_json::Value) {
        let env = recv(kernel).await;
        assert!(
            matches!(env.payload, Some(envelope::Payload::PluginRegister(_))),
            "expected PluginRegister, got {:?}",
            env.payload
        );
        kernel
            .send(
                "sync-client",
                Envelope {
                    payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck {
                        accepted: true,
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let env = recv(kernel).await;
        match env.payload {
            Some(envelope::Payload::Subscribe(Subscribe { event_types })) => {
                assert_eq!(event_types, vec!["plugin.sync.sync.delta".to_string()]);
            }
            other => panic!("expected Subscribe, got {other:?}"),
        }

        let env = recv(kernel).await;
        let req = match env.payload {
            Some(envelope::Payload::ActionRequest(req)) => req,
            other => panic!("expected snapshot ActionRequest, got {other:?}"),
        };
        assert_eq!(req.action, "sync_get_snapshot");
        kernel
            .send(
                "sync-client",
                Envelope {
                    payload: Some(envelope::Payload::ActionResponse(ActionResponse {
                        action_id: req.action_id,
                        status: ActionStatus::ActionOk as i32,
                        data_json: serde_json::to_vec(&snapshot).unwrap(),
                        error: String::new(),
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    /// Query the mirror through the plugin's own action; returns the decoded
    /// `{version, state}` JSON.
    async fn query_state(kernel: &mut VynkorClient, action_id: &str) -> serde_json::Value {
        kernel
            .send(
                "sync-client",
                Envelope {
                    payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                        action_id: action_id.to_string(),
                        action: "sync_client_get_state".into(),
                        params_json: b"{}".to_vec(),
                        timeout_ms: 0,
                        streaming: false,
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let env = recv(kernel).await;
        let resp = match env.payload {
            Some(envelope::Payload::ActionResponse(resp)) => resp,
            other => panic!("expected ActionResponse, got {other:?}"),
        };
        assert_eq!(resp.status, ActionStatus::ActionOk as i32, "{}", resp.error);
        serde_json::from_slice(&resp.data_json).unwrap()
    }

    async fn send_delta(kernel: &mut VynkorClient, delta: serde_json::Value) {
        kernel
            .send(
                "sync-client",
                Envelope {
                    payload: Some(envelope::Payload::Event(Event {
                        event_id: format!("ev-{}", unix_millis()),
                        event_type: "plugin.sync.sync.delta".into(),
                        payload_json: serde_json::to_vec(&delta).unwrap(),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // the loop acks the event after on_event succeeds — waiting for it
        // also guarantees the delta was applied before we assert on the mirror.
        let env = recv(kernel).await;
        assert!(
            matches!(env.payload, Some(envelope::Payload::EventAck(_))),
            "expected EventAck, got {:?}",
            env.payload
        );
    }

    async fn shutdown(
        kernel: &mut VynkorClient,
        loop_task: tokio::task::JoinHandle<Result<(), VynkorError>>,
    ) {
        kernel
            .send(
                "sync-client",
                Envelope {
                    payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                        reason: "test done".into(),
                        grace_seconds: 0,
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("loop did not exit after PluginShutdown")
            .unwrap()
            .unwrap();
    }

    fn pair() -> (VynkorClient, VynkorClient) {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        (
            VynkorClient::from_stream(plugin_side, None),
            VynkorClient::from_stream(kernel_side, None),
        )
    }

    #[tokio::test]
    async fn snapshot_seeds_mirror() {
        let handler = Arc::new(SyncClientHandler::new("dev".into(), 5000));
        let (client, mut kernel) = pair();

        let loop_task = tokio::spawn(serve_cycle(client, "", handler.clone(), 0));

        drive_registration(
            &mut kernel,
            serde_json::json!({"version": 1, "state": {"a": 1, "b": "x"}}),
        )
        .await;

        let state = query_state(&mut kernel, "q1").await;
        assert_eq!(state["version"], 1);
        assert_eq!(state["state"]["a"], 1);
        assert_eq!(state["state"]["b"], "x");

        shutdown(&mut kernel, loop_task).await;
    }

    #[tokio::test]
    async fn delta_updates_mirror() {
        let handler = Arc::new(SyncClientHandler::new("dev".into(), 5000));
        let (client, mut kernel) = pair();

        let loop_task = tokio::spawn(serve_cycle(client, "", handler.clone(), 0));
        drive_registration(
            &mut kernel,
            serde_json::json!({"version": 1, "state": {"a": 1}}),
        )
        .await;

        send_delta(
            &mut kernel,
            serde_json::json!({"op": "set", "key": "k", "value": 1, "version": 2, "updated_at": 123}),
        )
        .await;

        let state = query_state(&mut kernel, "q1").await;
        assert_eq!(state["version"], 2);
        assert_eq!(state["state"]["k"], 1);
        assert_eq!(state["state"]["a"], 1, "snapshot key lost after set delta");

        shutdown(&mut kernel, loop_task).await;
    }

    #[tokio::test]
    async fn stale_delta_is_skipped() {
        let handler = Arc::new(SyncClientHandler::new("dev".into(), 5000));
        let (client, mut kernel) = pair();

        let loop_task = tokio::spawn(serve_cycle(client, "", handler.clone(), 0));
        drive_registration(
            &mut kernel,
            serde_json::json!({"version": 5, "state": {"a": 1}}),
        )
        .await;

        // version 3 <= mirror version 5 — already covered, must be ignored.
        send_delta(
            &mut kernel,
            serde_json::json!({"op": "set", "key": "k", "value": 9, "version": 3, "updated_at": 123}),
        )
        .await;

        let state = query_state(&mut kernel, "q1").await;
        assert_eq!(
            state["version"], 5,
            "stale delta advanced the mirror version"
        );
        assert!(
            state["state"].get("k").is_none(),
            "stale delta was applied: {state}"
        );
        assert_eq!(state["state"]["a"], 1);

        shutdown(&mut kernel, loop_task).await;
    }

    #[tokio::test]
    async fn reconnect_pulls_fresh_snapshot() {
        let handler = Arc::new(SyncClientHandler::new("dev".into(), 5000));

        // first cycle: snapshot A
        let (client_a, mut kernel_a) = pair();
        let task_a = tokio::spawn(serve_cycle(client_a, "", handler.clone(), 0));
        drive_registration(
            &mut kernel_a,
            serde_json::json!({"version": 1, "state": {"old": true}}),
        )
        .await;
        let state = query_state(&mut kernel_a, "q1").await;
        assert_eq!(state["state"]["old"], true);
        shutdown(&mut kernel_a, task_a).await;

        // second cycle on a fresh pair: snapshot B must replace the mirror
        // (and drive_registration already asserts a fresh Subscribe was sent).
        let (client_b, mut kernel_b) = pair();
        let task_b = tokio::spawn(serve_cycle(client_b, "", handler.clone(), 0));
        drive_registration(
            &mut kernel_b,
            serde_json::json!({"version": 7, "state": {"fresh": "yes"}}),
        )
        .await;

        let state = query_state(&mut kernel_b, "q2").await;
        assert_eq!(state["version"], 7);
        assert_eq!(state["state"]["fresh"], "yes");
        assert!(
            state["state"].get("old").is_none(),
            "stale key survived reconnect: {state}"
        );

        shutdown(&mut kernel_b, task_b).await;
    }

    #[tokio::test]
    async fn heartbeat_emits_sync_set() {
        let handler = Arc::new(SyncClientHandler::new("test-device".into(), 5000));
        let (client, mut kernel) = pair();

        let loop_task = tokio::spawn(run_sync_client_loop(client, handler.clone(), 1));

        // first heartbeat tick fires immediately; expect a sync_set
        // ActionRequest within a couple of seconds.
        let env = tokio::time::timeout(Duration::from_secs(3), kernel.recv())
            .await
            .expect("timed out waiting for heartbeat")
            .unwrap();
        let req = match env.payload {
            Some(envelope::Payload::ActionRequest(req)) => req,
            other => panic!("expected heartbeat ActionRequest, got {other:?}"),
        };
        assert_eq!(req.action, "sync_set");
        let params: serde_json::Value = serde_json::from_slice(&req.params_json).unwrap();
        assert_eq!(params["key"], "heartbeat.test-device");
        assert_eq!(params["value"]["device_id"], "test-device");
        assert!(
            params["value"]["ts"].is_u64(),
            "heartbeat ts missing: {params}"
        );

        shutdown(&mut kernel, loop_task).await;
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
