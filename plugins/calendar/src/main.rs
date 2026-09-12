//! `calendar` plugin — event CRUD over the `database` plugin plus reminders,
//! gated by nothing of its own beyond what it calls (`PERMISSION_STORAGE`,
//! `PERMISSION_EVENT_PUBLISH`, `PERMISSION_NOTIFY`). See README.md.
//!
//! Serve-loop architecture (sync-client model): the loop task exclusively
//! owns the `VynkorClient` and is the single reader of the connection, so no
//! inbound frame is ever discarded. Action handlers and the periodic
//! reminder scan run as spawned tasks that reach `database`/`notify` through
//! the [`Rpc`] proxy channel; replies and fire-and-forget events flow back
//! through an outbound channel the loop drains. This matters precisely
//! because of the timer: a scan started by a tick must never eat a user
//! request arriving mid-scan (`send_action`'s discard-while-waiting would).
//!
//! The first tick fires immediately, which doubles as the startup catch-up:
//! reminders that came due while the plugin was down fire once with
//! `late: true`.

use std::collections::HashMap;
use std::sync::Arc;

use calendar_plugin::{handle_action, scan_due, store, Config, Rpc, RpcCall};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, EventPublish, PluginManifest,
    Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "calendar";
const PLUGIN_VERSION: &str = "0.1.1";
const ACTIONS: [&str; 7] = [
    "event_create",
    "event_get",
    "event_list",
    "event_update",
    "event_delete",
    "calendar_ics_import",
    "calendar_ics_export",
];

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec![
            "PERMISSION_STORAGE".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
            "PERMISSION_NOTIFY".into(),
        ],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
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

    // A zero period would panic tokio's interval; when scanning is disabled
    // the branch below is never polled, so the placeholder period is inert.
    let scanning = config.scan_secs > 0;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        if scanning { config.scan_secs.max(1) } else { 3600 },
    ));

    println!("[{PLUGIN_ID}] registered with kernel");
    if scanning {
        println!("[{PLUGIN_ID}] reminder scan every {}s", config.scan_secs);
    }

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
                        // calendar declares no event subscriptions; ack
                        // defensively so the kernel doesn't retry anything
                        // unexpectedly delivered.
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
            _ = interval.tick(), if scanning => {
                let rpc = rpc.clone();
                let out = outbound_tx.clone();
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    match scan_due(rpc, out, &config, store::now_ms()).await {
                        Ok(n) if n > 0 => println!("[{PLUGIN_ID}] fired {n} reminder(s)"),
                        Ok(_) => {}
                        Err(e) => eprintln!("[{PLUGIN_ID}] reminder scan failed: {e}"),
                    }
                });
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
    type Notified = Arc<Mutex<Vec<Value>>>;

    /// In-memory stand-in for the `database` plugin (same KV semantics the
    /// notes tests use) plus a `notify_send` recorder.
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
        notified: Notified,
    }

    impl Shim {
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

        async fn notified(&self) -> Vec<Value> {
            self.notified.lock().await.clone()
        }
    }

    /// Start the real `serve` loop against a fake kernel over a socket pair.
    /// The shim answers registration, database calls, event publishes and
    /// records `notify_send` deliveries.
    async fn start_plugin(config: Config) -> Shim {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        tokio::spawn(async move {
            let _ = serve(plugin_client, config).await;
        });

        let (tx, rx) = mpsc::channel::<Cmd>(16);
        let published: Published = Arc::new(Mutex::new(Vec::new()));
        let notified: Notified = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_shim(kernel_client, rx, published.clone(), notified.clone()));
        Shim { tx, published, notified }
    }

    async fn run_shim(
        mut kernel: VynkorClient,
        mut rx: mpsc::Receiver<Cmd>,
        published: Published,
        notified: Notified,
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
                    let _ = kernel.send("calendar", Envelope {
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
                            let params: Value = serde_json::from_slice(&req.params_json)
                                .unwrap_or(Value::Null);
                            let outcome = if req.action == "notify_send" {
                                notified.lock().await.push(params);
                                Ok(serde_json::json!({"id": "n1", "delivered": true}))
                            } else {
                                db.handle(&req.action, params)
                            };
                            let resp = match outcome {
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
                            let _ = kernel.send("calendar", Envelope {
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
                            let _ = kernel.send("calendar", Envelope {
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
                            let _ = kernel.send("calendar", Envelope {
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
                            let _ = kernel.send("calendar", env).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// The plugin sends responses before events and scans happen on their
    /// own timer, so assertions on background activity poll briefly instead
    /// of checking once.
    async fn wait_for_published(
        shim: &Shim,
        pred: impl Fn(&(String, Value)) -> bool,
    ) -> Option<(String, Value)> {
        for _ in 0..120 {
            let pubs = shim.published().await;
            if let Some(found) = pubs.iter().find(|p| pred(p)) {
                return Some(found.clone());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }

    async fn wait_for_notified(shim: &Shim, title_contains: &str) -> bool {
        for _ in 0..120 {
            let notes = shim.notified().await;
            if notes.iter().any(|n| {
                n.get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.contains(title_contains))
            }) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    fn scan_config() -> Config {
        Config { db_timeout_ms: 5000, scan_secs: 1, notify_enabled: true }
    }

    #[tokio::test]
    async fn create_then_get_roundtrips() {
        let shim = start_plugin(Config::default()).await;

        let created = shim
            .call(
                "event_create",
                serde_json::json!({
                    "title": "Dentist",
                    "description": "checkup",
                    "start_ms": 1_000_000,
                    "end_ms": 1_800_000,
                    "tags": ["health"]
                }),
            )
            .await
            .unwrap();
        assert_eq!(created["id"], "1");
        assert_eq!(created["event"]["reminder_fired"], false);

        let got = shim.call("event_get", serde_json::json!({"id": "1"})).await.unwrap();
        assert_eq!(got["found"], true);
        assert_eq!(got["event"]["title"], "Dentist");
        assert_eq!(got["event"]["end_ms"], 1_800_000);

        let missing = shim.call("event_get", serde_json::json!({"id": "9"})).await.unwrap();
        assert_eq!(missing["found"], false);
    }

    #[tokio::test]
    async fn list_sorts_by_start_and_filters_range_and_tag() {
        let shim = start_plugin(Config::default()).await;
        for (title, start, tags) in [
            ("late", 5_000, vec!["work"]),
            ("early", 1_000, vec!["home"]),
            ("mid", 3_000, vec!["work", "urgent"]),
        ] {
            shim.call(
                "event_create",
                serde_json::json!({"title": title, "start_ms": start, "tags": tags}),
            )
            .await
            .unwrap();
        }

        let all = shim.call("event_list", serde_json::json!({})).await.unwrap();
        assert_eq!(all["total"], 3);
        let titles: Vec<&str> = all["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, vec!["early", "mid", "late"]);

        let ranged = shim
            .call(
                "event_list",
                serde_json::json!({"from_ms": 1_000, "to_ms": 3_000}),
            )
            .await
            .unwrap();
        assert_eq!(ranged["total"], 2);

        let work = shim.call("event_list", serde_json::json!({"tag": "work"})).await.unwrap();
        assert_eq!(work["total"], 2);

        let page = shim
            .call("event_list", serde_json::json!({"limit": 1, "offset": 1}))
            .await
            .unwrap();
        assert_eq!(page["total"], 3);
        assert_eq!(page["events"].as_array().unwrap().len(), 1);
        assert_eq!(page["events"][0]["title"], "mid");
    }

    #[tokio::test]
    async fn update_resets_reminder_fired_and_revalidates_end() {
        let shim = start_plugin(scan_config()).await;
        let now = store::now_ms();

        // Already-due reminder so the scan marks it fired.
        let created = shim
            .call(
                "event_create",
                serde_json::json!({
                    "title": "Sync",
                    "start_ms": now + 500,
                    "remind_before_ms": 1_000
                }),
            )
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap().to_string();

        let fired = wait_for_published(&shim, |(t, p)| {
            t == "due" && p.get("event_id").and_then(Value::as_str) == Some("1")
        })
        .await;
        assert!(fired.is_some(), "expected a due event within the scan window");

        let got = shim.call("event_get", serde_json::json!({"id": id})).await.unwrap();
        assert_eq!(got["event"]["reminder_fired"], true);

        // Moving the event resets the fired flag; the new time reminds again.
        let updated = shim
            .call(
                "event_update",
                serde_json::json!({"id": id, "start_ms": now + 3_600_000}),
            )
            .await
            .unwrap();
        assert_eq!(updated["updated"], true);
        assert_eq!(updated["event"]["reminder_fired"], false);

        // Resulting end < start is rejected against the patched document.
        let err = shim
            .call(
                "event_update",
                serde_json::json!({"id": id, "end_ms": now + 60_000}),
            )
            .await
            .unwrap_err();
        assert!(err.contains("end_ms"), "error was: {err}");

        let err = shim.call("event_update", serde_json::json!({"id": "99", "title": "x"})).await.unwrap_err();
        assert!(err.contains("event not found"), "error was: {err}");
    }

    #[tokio::test]
    async fn delete_is_idempotent_and_publishes_changed() {
        let shim = start_plugin(Config::default()).await;
        let created = shim
            .call(
                "event_create",
                serde_json::json!({"title": "bye", "start_ms": 1_000}),
            )
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap().to_string();

        let first = shim.call("event_delete", serde_json::json!({"id": id})).await.unwrap();
        assert_eq!(first["deleted"], true);

        let second = shim.call("event_delete", serde_json::json!({"id": id})).await.unwrap();
        assert_eq!(second["deleted"], false);

        let deleted = wait_for_published(&shim, |(t, p)| {
            t == "changed"
                && p.get("op").and_then(Value::as_str) == Some("deleted")
                && p.get("id").and_then(Value::as_str) == Some("1")
        })
        .await;
        assert!(deleted.is_some(), "changed(deleted) event missing");
    }

    #[tokio::test]
    async fn validation_errors_surface_as_action_errors() {
        let shim = start_plugin(Config::default()).await;

        // Absent title fails at parse time with a shape-naming error;
        // whitespace-only title reaches the explicit validation.
        let err = shim
            .call("event_create", serde_json::json!({"start_ms": 100}))
            .await
            .unwrap_err();
        assert!(err.contains("title"), "error was: {err}");

        let err = shim
            .call(
                "event_create",
                serde_json::json!({"title": "  ", "start_ms": 100}),
            )
            .await
            .unwrap_err();
        assert!(err.contains("non-empty title"), "error was: {err}");

        let err = shim
            .call(
                "event_create",
                serde_json::json!({"title": "t", "start_ms": 200, "end_ms": 100}),
            )
            .await
            .unwrap_err();
        assert!(err.contains("end_ms"), "error was: {err}");

        let err = shim.call("event_update", serde_json::json!({"id": "1"})).await.unwrap_err();
        assert!(err.contains("at least one of"), "error was: {err}");

        let err = shim
            .call("event_list", serde_json::json!({"limit": 0}))
            .await
            .unwrap_err();
        assert!(err.contains("limit"), "error was: {err}");
    }

    #[tokio::test]
    async fn unknown_action_is_action_error() {
        let shim = start_plugin(Config::default()).await;
        let err = shim.call("event_frobnicate", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("unknown action"), "error was: {err}");
    }

    #[tokio::test]
    async fn reminder_scan_fires_once_notifies_and_marks_fired() {
        let shim = start_plugin(scan_config()).await;
        let now = store::now_ms();

        // remind_at = start - lead = now - 1000 → due immediately, not late.
        let created = shim
            .call(
                "event_create",
                serde_json::json!({
                    "title": "Standup",
                    "start_ms": now + 2_000,
                    "remind_before_ms": 3_000
                }),
            )
            .await
            .unwrap();
        assert_eq!(created["id"], "1");

        let due = wait_for_published(&shim, |(t, p)| {
            t == "due" && p.get("event_id").and_then(Value::as_str) == Some("1")
        })
        .await
        .expect("expected a due event within the scan window");
        assert_eq!(due.1["late"], false);
        assert_eq!(due.1["title"], "Standup");

        assert!(
            wait_for_notified(&shim, "Standup").await,
            "expected a notify_send delivery mentioning Standup"
        );

        let got = shim.call("event_get", serde_json::json!({"id": "1"})).await.unwrap();
        assert_eq!(got["event"]["reminder_fired"], true);

        // Give the scanner more than one extra tick, then confirm at-most-once.
        tokio::time::sleep(Duration::from_millis(1_600)).await;
        let due_count = shim
            .published()
            .await
            .into_iter()
            .filter(|(t, p)| t == "due" && p["event_id"] == "1")
            .count();
        assert_eq!(due_count, 1, "reminder must not re-fire after being marked");
    }

    #[tokio::test]
    async fn ics_import_export_roundtrip() {
        use base64::Engine as _;
        use calendar_plugin::ics::{fmt_dt, MS_PER_DAY};

        let shim = start_plugin(Config::default()).await;

        // A VEVENT inside the import window ([now-1d, now+90d]) so the
        // importer materializes it; LOCATION folds into the description.
        let start_ms = store::now_ms() + 2 * MS_PER_DAY;
        let end_ms = start_ms + 3_600_000;
        let ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n\
             BEGIN:VEVENT\r\nUID:roundtrip-1@example.com\r\n\
             DTSTART:{}\r\nDTEND:{}\r\n\
             SUMMARY:Team Standup\r\n\
             DESCRIPTION:Daily sync with the team\r\n\
             LOCATION:Room 42\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
            fmt_dt(start_ms, false),
            fmt_dt(end_ms, false),
        );
        let ics_b64 = base64::engine::general_purpose::STANDARD.encode(ics.as_bytes());

        let imported = shim
            .call("calendar_ics_import", serde_json::json!({"ics_base64": ics_b64}))
            .await
            .unwrap();
        assert_eq!(imported["imported"], 1);
        assert_eq!(imported["updated"], 0);
        assert_eq!(imported["parsed"], 1);

        let list = shim.call("event_list", serde_json::json!({})).await.unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["events"][0]["title"], "Team Standup");
        assert_eq!(list["events"][0]["ics_uid"], "roundtrip-1@example.com");

        let exported = shim.call("calendar_ics_export", serde_json::json!({})).await.unwrap();
        let exported_b64 = exported["ics_base64"].as_str().unwrap();
        let exported_ics = String::from_utf8(
            base64::engine::general_purpose::STANDARD.decode(exported_b64).unwrap(),
        )
        .unwrap();

        assert!(exported_ics.contains("SUMMARY:Team Standup"), "exported: {exported_ics}");
        assert!(exported_ics.contains("UID:roundtrip-1@example.com"));
        assert!(exported_ics.contains("Daily sync with the team"));
        assert!(exported_ics.contains("Room 42"), "location folded into description");
    }
}
