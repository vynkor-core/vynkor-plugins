//! `notes` plugin — note CRUD over the `database` plugin, gated by nothing
//! of its own (callers need no storage permission; this plugin is the one
//! holding `PERMISSION_STORAGE`). See README.md.
//!
//! Serve-loop architecture (sync-client model): the loop task exclusively
//! owns the `VynkorClient` and is the single reader of the connection, so no
//! inbound frame is ever discarded. Each inbound `ActionRequest` is handled
//! in a spawned task that reaches `database` through the [`Rpc`] proxy
//! channel; replies and fire-and-forget change events flow back through an
//! outbound channel the loop drains. Notes is not hot-path, but even here
//! the proxy matters: `send_action`'s discard-while-waiting would otherwise
//! eat kernel pings arriving during a slow database round-trip.

use std::collections::HashMap;
use std::sync::Arc;

use notes_plugin::{handle_action, Config, Rpc, RpcCall};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, EventPublish, PluginManifest,
    Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "notes";
const PLUGIN_VERSION: &str = "0.1.0";
const ACTIONS: [&str; 5] =
    ["note_create", "note_get", "note_list", "note_update", "note_delete"];

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec![
            "PERMISSION_STORAGE".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
        action_specs: vynkor_plugin_manifest::action_specs(),
        ..Default::default()
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn action_response(
    action_id: String,
    status: ActionStatus,
    data_json: Vec<u8>,
    error: String,
) -> Envelope {
    Envelope {
        payload: Some(envelope::Payload::ActionResponse(ActionResponse {
            action_id,
            status: status as i32,
            data_json,
            error,
        })),
        ..Default::default()
    }
}

fn event_envelope(event_type: &str, payload: &Value) -> Envelope {
    Envelope {
        payload: Some(envelope::Payload::EventPublish(EventPublish {
            event_type: event_type.to_string(),
            payload_json: payload.to_string().into_bytes(),
        })),
        ..Default::default()
    }
}

async fn serve(mut client: VynkorClient, config: Config) -> Result<(), VynkorError> {
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client
        .register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token)
        .await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!(
            "registration rejected: {}",
            ack.reject_reason
        )));
    }
    println!("[{PLUGIN_ID}] registered with kernel");

    let config = Arc::new(config);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(64);
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCall>(64);
    let rpc = Rpc::new(rpc_tx);

    let mut pending: HashMap<String, (String, oneshot::Sender<Result<Value, String>>)> =
        HashMap::new();
    let mut seq: u64 = 0;

    loop {
        tokio::select! {
            env = client.recv() => {
                let env = match env {
                    Ok(env) => env,
                    Err(_) => break, // disconnect / EOF
                };
                match env.payload {
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
                    Some(envelope::Payload::Event(event)) => {
                        // notes declares no event subscriptions; ack defensively
                        // so the kernel doesn't retry anything unexpectedly
                        // delivered.
                        let _ = client.ack_event(&event.event_id).await;
                    }
                    Some(envelope::Payload::EventPublishAck(_)) => {
                        // Ack for our own fire-and-forget publishes.
                    }
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let rpc = rpc.clone();
                        let out = outbound_tx.clone();
                        let config = Arc::clone(&config);
                        tokio::spawn(async move {
                            match handle_action(rpc, &config, &req.action, &req.params_json)
                                .await
                            {
                                Ok(result) => {
                                    // Response first — the caller's reply never
                                    // waits on the best-effort publish after it.
                                    let _ = out
                                        .send(action_response(
                                            req.action_id,
                                            ActionStatus::ActionOk,
                                            result.data,
                                            String::new(),
                                        ))
                                        .await;
                                    if let Some(ev) = result.event {
                                        let _ = out
                                            .send(event_envelope(ev.event_type, &ev.payload))
                                            .await;
                                    }
                                }
                                Err(error) => {
                                    let _ = out
                                        .send(action_response(
                                            req.action_id,
                                            ActionStatus::ActionError,
                                            Vec::new(),
                                            error,
                                        ))
                                        .await;
                                }
                            }
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        if let Some((action, reply)) = pending.remove(&resp.action_id) {
                            let result = if resp.status == ActionStatus::ActionOk as i32 {
                                serde_json::from_slice::<Value>(&resp.data_json)
                                    .map_err(|e| format!("malformed payload: {e}"))
                            } else {
                                Err(format!("{action} failed: {}", resp.error))
                            };
                            let _ = reply.send(result);
                        }
                    }
                    other => {
                        println!("[{PLUGIN_ID}] unhandled message: {other:?}");
                    }
                }
            }
            Some(env) = outbound_rx.recv() => {
                let _ = client.send("kernel", env).await;
            }
            Some(call) = rpc_rx.recv() => {
                seq += 1;
                let action_id = format!("rpc-{seq}");
                pending.insert(action_id.clone(), (call.action.clone(), call.reply));
                let env = Envelope {
                    payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                        action_id,
                        action: call.action,
                        params_json: call.params_json,
                        timeout_ms: call.timeout_ms,
                        streaming: false,
                        ..Default::default()
                    })),
                    ..Default::default()
                };
                let _ = client.send("kernel", env).await;
            }
        }
    }

    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let config = Config::from_env();
    let client = VynkorClient::connect_from_env().await?;
    serve(client, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap as StdHashMap};
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;
    use vynkor_sdk::proto::{
        ActionResponse as ProtoActionResponse, EventPublishAck, EventPublishStatus,
        PluginRegisterAck,
    };

    type Published = Arc<Mutex<Vec<(String, Value)>>>;

    /// In-memory stand-in for the `database` plugin: enough KV semantics for
    /// the notes contract (`db_incr`/`db_set`/`db_get`/`db_keys`/
    /// `db_batch_get`/`db_delete`). Exercises the real wire shapes end to
    /// end without a live kernel.
    #[derive(Default)]
    struct FakeDb {
        kv: BTreeMap<String, Value>,
    }

    impl FakeDb {
        fn handle(&mut self, action: &str, params: Value) -> Result<Value, String> {
            fn key(p: &Value) -> String {
                p.get("key").and_then(Value::as_str).unwrap_or_default().to_string()
            }
            match action {
                "db_incr" => {
                    let k = key(&params);
                    let delta = params.get("delta").and_then(Value::as_i64).unwrap_or(1);
                    let cur = self.kv.get(&k).and_then(Value::as_i64).unwrap_or(0);
                    let next = cur + delta;
                    self.kv.insert(k, serde_json::json!(next));
                    Ok(serde_json::json!({"ok": true, "value": next}))
                }
                "db_set" => {
                    let k = key(&params);
                    self.kv.insert(k, params.get("value").cloned().unwrap_or(Value::Null));
                    Ok(serde_json::json!({"ok": true}))
                }
                "db_get" => {
                    let k = key(&params);
                    match self.kv.get(&k) {
                        Some(v) => Ok(serde_json::json!({"found": true, "value": v})),
                        None => Ok(serde_json::json!({"found": false, "value": null})),
                    }
                }
                "db_keys" => {
                    let prefix = params.get("prefix").and_then(Value::as_str).unwrap_or("");
                    let keys: Vec<&String> =
                        self.kv.keys().filter(|k| k.starts_with(prefix)).collect();
                    Ok(serde_json::json!({ "keys": keys }))
                }
                "db_batch_get" => {
                    let mut values = serde_json::Map::new();
                    if let Some(keys) = params.get("keys").and_then(Value::as_array) {
                        for k in keys {
                            if let Some(k) = k.as_str() {
                                values.insert(
                                    k.to_string(),
                                    self.kv.get(k).cloned().unwrap_or(Value::Null),
                                );
                            }
                        }
                    }
                    Ok(serde_json::json!({"values": values}))
                }
                "db_delete" => {
                    let k = key(&params);
                    Ok(serde_json::json!({"deleted": self.kv.remove(&k).is_some()}))
                }
                other => Err(format!("fake db: unknown action {other}")),
            }
        }
    }

    enum Cmd {
        Call { action: String, params: Value, reply: oneshot::Sender<Result<Value, String>> },
    }

    struct Shim {
        tx: mpsc::Sender<Cmd>,
        published: Published,
    }

    impl Shim {
        /// Invoke one of the plugin's actions through the fake kernel and
        /// await its reply. `Err` mirrors an `ACTION_ERROR`.
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.tx
                .send(Cmd::Call { action: action.into(), params, reply: reply_tx })
                .await
                .expect("shim loop died");
            tokio::time::timeout(Duration::from_secs(5), reply_rx)
                .await
                .expect("timed out waiting for plugin reply")
                .expect("shim dropped reply channel")
        }

        async fn published(&self) -> Vec<(String, Value)> {
            self.published.lock().await.clone()
        }
    }

    /// Start the real `serve` loop against a fake kernel over a socket pair.
    /// The shim answers registration, database calls and event publishes.
    async fn start_plugin(config: Config) -> Shim {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        tokio::spawn(async move {
            let _ = serve(plugin_client, config).await;
        });

        let (tx, rx) = mpsc::channel::<Cmd>(16);
        let published: Published = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_shim(kernel_client, rx, published.clone()));
        Shim { tx, published }
    }

    async fn run_shim(
        mut kernel: VynkorClient,
        mut rx: mpsc::Receiver<Cmd>,
        published: Published,
    ) {
        let mut db = FakeDb::default();
        let mut pending: StdHashMap<String, oneshot::Sender<Result<Value, String>>> =
            StdHashMap::new();
        let mut seq: u64 = 0;

        // Registration handshake FIRST, before the command loop: the
        // plugin's register_full treats the very next inbound frame as the
        // ack, so a test command racing ahead of PluginRegister would kill
        // the plugin with "expected PluginRegisterAck". Commands queue in
        // the buffered `rx` until this completes.
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for plugin registration")
                .expect("plugin stream closed before registration");
            match env.payload {
                Some(envelope::Payload::PluginRegister(_)) => {
                    let _ = kernel.send("notes", Envelope {
                        payload: Some(envelope::Payload::PluginRegisterAck(
                            PluginRegisterAck { accepted: true, ..Default::default() },
                        )),
                        ..Default::default()
                    }).await;
                    break;
                }
                _ => continue,
            }
        }

        loop {
            tokio::select! {
                env = kernel.recv() => {
                    let env = match env { Ok(e) => e, Err(_) => break };
                    match env.payload {
                        Some(envelope::Payload::ActionRequest(req)) => {
                            // Outbound database.* call from the plugin.
                            let params: Value =
                                serde_json::from_slice(&req.params_json).unwrap_or(Value::Null);
                            let resp = match db.handle(&req.action, params) {
                                Ok(v) => ProtoActionResponse {
                                    action_id: req.action_id,
                                    status: ActionStatus::ActionOk as i32,
                                    data_json: serde_json::to_vec(&v).unwrap(),
                                    error: String::new(),
                                },
                                Err(e) => ProtoActionResponse {
                                    action_id: req.action_id,
                                    status: ActionStatus::ActionError as i32,
                                    data_json: Vec::new(),
                                    error: e,
                                },
                            };
                            let _ = kernel.send("notes", Envelope {
                                payload: Some(envelope::Payload::ActionResponse(resp)),
                                ..Default::default()
                            }).await;
                        }
                        Some(envelope::Payload::ActionResponse(resp)) => {
                            if let Some(tx) = pending.remove(&resp.action_id) {
                                let result = if resp.status == ActionStatus::ActionOk as i32 {
                                    serde_json::from_slice::<Value>(&resp.data_json)
                                        .map_err(|e| format!("malformed payload: {e}"))
                                } else {
                                    Err(resp.error)
                                };
                                let _ = tx.send(result);
                            }
                        }
                        Some(envelope::Payload::EventPublish(ev)) => {
                            published.lock().await.push((
                                ev.event_type.clone(),
                                serde_json::from_slice(&ev.payload_json).unwrap_or(Value::Null),
                            ));
                            let _ = kernel.send("notes", Envelope {
                                payload: Some(envelope::Payload::EventPublishAck(EventPublishAck {
                                    event_id: format!("ev-{seq}"),
                                    status: EventPublishStatus::EventPublishOk as i32,
                                    error: String::new(),
                                })),
                                ..Default::default()
                            }).await;
                            seq += 1;
                        }
                        Some(envelope::Payload::Ping(ping)) => {
                            let _ = kernel.send("notes", Envelope {
                                payload: Some(envelope::Payload::Pong(Pong {
                                    original_timestamp: ping.timestamp,
                                    server_timestamp: unix_millis(),
                                })),
                                ..Default::default()
                            }).await;
                        }
                        Some(envelope::Payload::PluginShutdown(_)) => break,
                        _ => {}
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Call { action, params, reply }) => {
                            seq += 1;
                            let action_id = format!("t-{seq}");
                            pending.insert(action_id.clone(), reply);
                            let env = Envelope {
                                payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                                    action_id,
                                    action,
                                    params_json: serde_json::to_vec(&params).unwrap(),
                                    timeout_ms: 0,
                                    streaming: false,
                                    caller_plugin_id: "tester".into(),
                                })),
                                ..Default::default()
                            };
                            let _ = kernel.send("notes", env).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// The plugin sends the response before the change-event envelope, so a
    /// just-finished `call` may race the recording — poll briefly instead of
    /// asserting immediately.
    async fn wait_for_published(shim: &Shim, op: &str, id: &str) -> bool {
        for _ in 0..40 {
            let pubs = shim.published().await;
            if pubs.iter().any(|(t, p)| {
                t == "changed"
                    && p.get("op").and_then(Value::as_str) == Some(op)
                    && p.get("id").and_then(Value::as_str) == Some(id)
            }) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn create_then_get_roundtrips() {
        let shim = start_plugin(Config::default()).await;

        let created = shim
            .call(
                "note_create",
                serde_json::json!({"title": "Shopping", "body": "milk", "tags": [" errands "]}),
            )
            .await
            .unwrap();
        assert_eq!(created["id"], "1");
        assert_eq!(created["note"]["title"], "Shopping");
        assert_eq!(created["note"]["tags"][0], "errands");

        let got = shim.call("note_get", serde_json::json!({"id": "1"})).await.unwrap();
        assert_eq!(got["found"], true);
        assert_eq!(got["note"]["body"], "milk");
        assert!(got["note"]["created_at_ms"].is_i64());

        assert!(wait_for_published(&shim, "created", "1").await, "changed event missing");
    }

    #[tokio::test]
    async fn create_assigns_monotonic_ids() {
        let shim = start_plugin(Config::default()).await;
        let a = shim.call("note_create", serde_json::json!({"body": "a"})).await.unwrap();
        let b = shim.call("note_create", serde_json::json!({"body": "b"})).await.unwrap();
        assert_eq!(a["id"], "1");
        assert_eq!(b["id"], "2");
    }

    #[tokio::test]
    async fn list_sorts_filters_paginates() {
        let shim = start_plugin(Config::default()).await;
        for (body, tags) in [
            ("first", vec!["work"]),
            ("second", vec!["home"]),
            ("third", vec!["work", "urgent"]),
        ] {
            shim.call(
                "note_create",
                serde_json::json!({"body": body, "tags": tags}),
            )
            .await
            .unwrap();
        }

        // Newest first (same-millisecond creates tie-break on id desc).
        let all = shim.call("note_list", serde_json::json!({})).await.unwrap();
        assert_eq!(all["total"], 3);
        let bodies: Vec<&str> = all["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["body"].as_str().unwrap())
            .collect();
        assert_eq!(bodies, vec!["third", "second", "first"]);

        let work = shim.call("note_list", serde_json::json!({"tag": "work"})).await.unwrap();
        assert_eq!(work["total"], 2);

        let page = shim
            .call("note_list", serde_json::json!({"limit": 1, "offset": 1}))
            .await
            .unwrap();
        assert_eq!(page["total"], 3);
        assert_eq!(page["notes"].as_array().unwrap().len(), 1);
        assert_eq!(page["notes"][0]["body"], "second");
    }

    #[tokio::test]
    async fn update_patches_fields_and_bumps_updated_at() {
        let shim = start_plugin(Config::default()).await;
        let created = shim
            .call("note_create", serde_json::json!({"title": "t", "body": "old"}))
            .await
            .unwrap();

        let updated = shim
            .call("note_update", serde_json::json!({"id": created["id"], "body": "new"}))
            .await
            .unwrap();
        assert_eq!(updated["updated"], true);
        assert_eq!(updated["note"]["title"], "t", "untouched field must survive");
        assert_eq!(updated["note"]["body"], "new");
        assert!(
            updated["note"]["updated_at_ms"].as_i64().unwrap()
                >= updated["note"]["created_at_ms"].as_i64().unwrap()
        );

        let got = shim.call("note_get", serde_json::json!({"id": created["id"]})).await.unwrap();
        assert_eq!(got["note"]["body"], "new");

        assert!(wait_for_published(&shim, "updated", "1").await, "changed event missing");
    }

    #[tokio::test]
    async fn update_missing_note_is_action_error() {
        let shim = start_plugin(Config::default()).await;
        let err = shim
            .call("note_update", serde_json::json!({"id": "99", "body": "x"}))
            .await
            .unwrap_err();
        assert!(err.contains("note not found"), "error was: {err}");
    }

    #[tokio::test]
    async fn delete_is_idempotent_and_publishes_only_real_deletes() {
        let shim = start_plugin(Config::default()).await;
        let created = shim
            .call("note_create", serde_json::json!({"body": "bye"}))
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap().to_string();

        let first = shim.call("note_delete", serde_json::json!({"id": id})).await.unwrap();
        assert_eq!(first["deleted"], true);

        let second = shim.call("note_delete", serde_json::json!({"id": id})).await.unwrap();
        assert_eq!(second["deleted"], false);

        assert!(wait_for_published(&shim, "deleted", "1").await, "delete event missing");
        // Exactly one deleted event for id 1 (no duplicate from the retry).
        let deletes = shim
            .published()
            .await
            .into_iter()
            .filter(|(t, p)| t == "changed" && p["op"] == "deleted" && p["id"] == "1")
            .count();
        assert_eq!(deletes, 1);
    }

    #[tokio::test]
    async fn validation_errors_surface_as_action_errors() {
        let shim = start_plugin(Config::default()).await;

        let err = shim.call("note_create", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("non-empty title or body"), "error was: {err}");

        let err = shim
            .call("note_list", serde_json::json!({"limit": 0}))
            .await
            .unwrap_err();
        assert!(err.contains("limit"), "error was: {err}");

        let err = shim.call("note_update", serde_json::json!({"id": "1"})).await.unwrap_err();
        assert!(err.contains("at least one of"), "error was: {err}");
    }

    #[tokio::test]
    async fn unknown_action_is_action_error() {
        let shim = start_plugin(Config::default()).await;
        let err = shim.call("note_frobnicate", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("unknown action"), "error was: {err}");
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
