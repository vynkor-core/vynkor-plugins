//! `scheduler` plugin — once/cron schedules over the `database` plugin,
//! gated by nothing of its own beyond what it calls (`PERMISSION_STORAGE`,
//! `PERMISSION_EVENT_PUBLISH`). See README.md.
//!
//! Serve-loop architecture (calendar/sync-client model): the loop task
//! exclusively owns the `VynkorClient` and is the single reader of the
//! connection, so no inbound frame is ever discarded. Action handlers and
//! the periodic scan run as spawned tasks that reach `database` and
//! fired-action targets through the [`Rpc`] proxy channel; replies and
//! fire-and-forget events flow back through an outbound channel the loop
//! drains. A scan started by a tick uses several sequential IPC round-trips,
//! and direct `send_action` would silently discard user requests arriving
//! mid-scan.
//!
//! The first tick fires immediately, which doubles as the startup catch-up:
//! one-shots that came due while the plugin was down fire once with
//! `late: true`; cron schedules resume from the first occurrence after now.

use std::collections::HashMap;
use std::sync::Arc;

use scheduler_plugin::{handle_action, scan_due, store, Config, Rpc, RpcCall};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, EventPublish, PluginManifest,
    Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "scheduler";
const PLUGIN_VERSION: &str = "0.1.0";
const ACTIONS: [&str; 4] = [
    "schedule_set",
    "schedule_get",
    "schedule_list",
    "schedule_delete",
];

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

    // A zero period would panic tokio's interval; when scanning is disabled
    // the branch below is never polled, so the placeholder period is inert.
    let scanning = config.scan_secs > 0;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(if scanning {
        config.scan_secs.max(1)
    } else {
        3600
    }));

    println!("[{PLUGIN_ID}] registered with kernel");
    if scanning {
        println!("[{PLUGIN_ID}] schedule scan every {}s", config.scan_secs);
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
                        // scheduler declares no event subscriptions; ack
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
                            match handle_action(
                                rpc,
                                &config,
                                &req.action,
                                &req.params_json,
                                store::now_ms(),
                            )
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
                        Ok(n) if n > 0 => println!("[{PLUGIN_ID}] fired {n} schedule(s)"),
                        Ok(_) => {}
                        Err(e) => eprintln!("[{PLUGIN_ID}] schedule scan failed: {e}"),
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
    /// Kernel-routed calls the plugin fired in action mode: `(action, params)`.
    type Dispatched = Arc<Mutex<Vec<(String, Value)>>>;

    /// In-memory stand-in for the `database` plugin (same KV semantics the
    /// notes/calendar tests use).
    #[derive(Default)]
    struct FakeDb {
        kv: BTreeMap<String, Value>,
    }

    impl FakeDb {
        fn handle(&mut self, action: &str, params: Value) -> Result<Value, String> {
            fn key(p: &Value) -> String {
                p.get("key")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
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
                    self.kv
                        .insert(k, params.get("value").cloned().unwrap_or(Value::Null));
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
        Call {
            action: String,
            params: Value,
            reply: oneshot::Sender<Result<Value, String>>,
        },
    }

    struct Shim {
        tx: mpsc::Sender<Cmd>,
        published: Published,
        dispatched: Dispatched,
    }

    impl Shim {
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.tx
                .send(Cmd::Call {
                    action: action.into(),
                    params,
                    reply: reply_tx,
                })
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

        async fn dispatched(&self) -> Vec<(String, Value)> {
            self.dispatched.lock().await.clone()
        }
    }

    /// Start the real `serve` loop against a fake kernel over a socket pair.
    /// The shim answers registration, database calls and event publishes,
    /// records every non-db action call, and fails exactly `fail_please`.
    async fn start_plugin(config: Config) -> Shim {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        tokio::spawn(async move {
            let _ = serve(plugin_client, config).await;
        });

        let (tx, rx) = mpsc::channel::<Cmd>(16);
        let published: Published = Arc::new(Mutex::new(Vec::new()));
        let dispatched: Dispatched = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_shim(
            kernel_client,
            rx,
            published.clone(),
            dispatched.clone(),
        ));
        Shim {
            tx,
            published,
            dispatched,
        }
    }

    async fn run_shim(
        mut kernel: VynkorClient,
        mut rx: mpsc::Receiver<Cmd>,
        published: Published,
        dispatched: Dispatched,
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
                    let _ = kernel
                        .send(
                            "scheduler",
                            Envelope {
                                payload: Some(envelope::Payload::PluginRegisterAck(
                                    PluginRegisterAck {
                                        accepted: true,
                                        ..Default::default()
                                    },
                                )),
                                ..Default::default()
                            },
                        )
                        .await;
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
                            let outcome = match req.action.as_str() {
                                a if a.starts_with("db_") => db.handle(a, params),
                                "fail_please" => Err("nope".to_string()),
                                other => {
                                    dispatched.lock().await.push((other.to_string(), params));
                                    Ok(serde_json::json!({"ok": true}))
                                }
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
                            let _ = kernel.send("scheduler", Envelope {
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
                            let _ = kernel.send("scheduler", Envelope {
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
                            let _ = kernel.send("scheduler", Envelope {
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
                            let _ = kernel.send("scheduler", env).await;
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

    fn no_scan_config() -> Config {
        Config {
            db_timeout_ms: 5000,
            scan_secs: 0,
        }
    }

    fn scan_config() -> Config {
        Config {
            db_timeout_ms: 5000,
            scan_secs: 1,
        }
    }

    #[tokio::test]
    async fn set_get_roundtrips_with_client_id_and_replace_resets_state() {
        let shim = start_plugin(no_scan_config()).await;

        let created = shim
            .call(
                "schedule_set",
                serde_json::json!({
                    "id": "backup-db",
                    "name": "nightly backup",
                    "once": {"delay_ms": 60_000},
                    "action": {"name": "notify_send", "params": {"title": "backup"}}
                }),
            )
            .await
            .unwrap();
        assert_eq!(created["created"], true);
        assert_eq!(created["id"], "backup-db");
        let at = created["schedule"]["trigger"]["at_ms"].as_i64().unwrap();
        assert!(
            at > store::now_ms() + 55_000,
            "delay resolved into the future: {at}"
        );

        let got = shim
            .call("schedule_get", serde_json::json!({"id": "backup-db"}))
            .await
            .unwrap();
        assert_eq!(got["found"], true);
        assert_eq!(got["schedule"]["name"], "nightly backup");
        assert_eq!(got["schedule"]["enabled"], true);
        assert_eq!(got["schedule"]["fired_once"], false);

        let missing = shim
            .call("schedule_get", serde_json::json!({"id": "nope"}))
            .await
            .unwrap();
        assert_eq!(missing["found"], false);

        // Re-setting the same id replaces the document and resets runtime
        // state (fresh schedule semantics).
        let replaced = shim
            .call(
                "schedule_set",
                serde_json::json!({
                    "id": "backup-db",
                    "cron": {"expr": "0 3 * * *"},
                    "event": {}
                }),
            )
            .await
            .unwrap();
        assert_eq!(replaced["created"], false);
        assert_eq!(replaced["schedule"]["trigger"]["type"], "cron");
        assert_eq!(replaced["schedule"]["fire_count"], 0);
    }

    #[tokio::test]
    async fn list_sorts_by_next_fire_and_paginates() {
        let shim = start_plugin(no_scan_config()).await;
        // Scanning is off, so nothing fires mid-test and next_fire stays
        // stable: overdue one-shot < future one-shot < disabled (null last).
        shim.call(
            "schedule_set",
            serde_json::json!({"id": "future", "once": {"at_ms": 5_000}, "event": {}}),
        )
        .await
        .unwrap();
        shim.call(
            "schedule_set",
            serde_json::json!({"id": "overdue", "once": {"at_ms": 1_000}, "event": {}}),
        )
        .await
        .unwrap();
        shim.call(
            "schedule_set",
            serde_json::json!({
                "id": "off",
                "enabled": false,
                "once": {"at_ms": 2_000},
                "event": {}
            }),
        )
        .await
        .unwrap();

        let all = shim
            .call("schedule_list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(all["total"], 3);
        let ids: Vec<&str> = all["schedules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["overdue", "future", "off"]);

        let page = shim
            .call(
                "schedule_list",
                serde_json::json!({"limit": 1, "offset": 1}),
            )
            .await
            .unwrap();
        assert_eq!(page["total"], 3);
        let page_ids: Vec<&str> = page["schedules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(page_ids, vec!["future"]);
    }

    #[tokio::test]
    async fn once_delay_fires_publishes_and_marks_done_at_most_once() {
        let shim = start_plugin(scan_config()).await;

        let created = shim
            .call(
                "schedule_set",
                serde_json::json!({
                    "name": "ping me",
                    "once": {"delay_ms": 800},
                    "event": {"payload": {"hello": true}}
                }),
            )
            .await
            .unwrap();
        assert_eq!(created["id"], "1");

        // With a 1s scan granularity an 800ms delay lands past its deadline
        // by the time the second tick runs — late=true is the honest answer,
        // not a bug.
        let fired = wait_for_published(&shim, |(t, p)| {
            t == "fired" && p.get("schedule_id").and_then(Value::as_str) == Some("1")
        })
        .await
        .expect("expected a fired event within the scan window");
        assert_eq!(fired.1["late"], true);
        assert_eq!(fired.1["payload"]["hello"], true);
        assert_eq!(fired.1["fire_count"], 1);

        let got = shim
            .call("schedule_get", serde_json::json!({"id": "1"}))
            .await
            .unwrap();
        assert_eq!(got["schedule"]["fired_once"], true);
        assert_eq!(got["schedule"]["fire_count"], 1);

        // Give the scanner more than one extra tick, then confirm
        // at-most-once.
        tokio::time::sleep(Duration::from_millis(1_600)).await;
        let count = shim
            .published()
            .await
            .into_iter()
            .filter(|(t, p)| t == "fired" && p["schedule_id"] == "1")
            .count();
        assert_eq!(count, 1, "one-shot must not re-fire after being marked");
    }

    #[tokio::test]
    async fn past_due_one_shot_fires_with_late_flag() {
        let shim = start_plugin(scan_config()).await;
        let now = store::now_ms();
        shim.call(
            "schedule_set",
            serde_json::json!({"id": "catchup", "once": {"at_ms": now - 5_000}, "event": {}}),
        )
        .await
        .unwrap();

        let fired = wait_for_published(&shim, |(t, p)| {
            t == "fired" && p.get("schedule_id").and_then(Value::as_str) == Some("catchup")
        })
        .await
        .expect("expected the missed one-shot to fire during startup catch-up");
        assert_eq!(fired.1["late"], true);
        assert_eq!(fired.1["scheduled_for_ms"], now - 5_000);
    }

    #[tokio::test]
    async fn action_mode_dispatches_kernel_call_and_records_errors() {
        let shim = start_plugin(scan_config()).await;
        let now = store::now_ms();

        // Fails: shim answers fail_please with an error → last_error lands
        // on the document, fire still counts (mark happened before dispatch).
        shim.call(
            "schedule_set",
            serde_json::json!({
                "id": "will-fail",
                "once": {"at_ms": now - 100},
                "action": {"name": "fail_please", "params": {}}
            }),
        )
        .await
        .unwrap();
        for _ in 0..120 {
            let got = shim
                .call("schedule_get", serde_json::json!({"id": "will-fail"}))
                .await
                .unwrap();
            if got["schedule"]["last_error"].is_string() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let got = shim
            .call("schedule_get", serde_json::json!({"id": "will-fail"}))
            .await
            .unwrap();
        assert!(
            got["schedule"]["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("nope")),
            "expected last_error to surface the dispatch failure: {}",
            got["schedule"]["last_error"]
        );

        // Succeeds: notify_send reaches the recorder.
        shim.call(
            "schedule_set",
            serde_json::json!({
                "id": "will-work",
                "once": {"at_ms": now - 100},
                "action": {"name": "notify_send", "params": {"title": "hi"}}
            }),
        )
        .await
        .unwrap();
        for _ in 0..120 {
            if shim
                .dispatched()
                .await
                .iter()
                .any(|(a, p)| a == "notify_send" && p["title"] == "hi")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            shim.dispatched()
                .await
                .iter()
                .any(|(a, p)| a == "notify_send" && p["title"] == "hi"),
            "expected the scheduled action to be dispatched"
        );
        let got = shim
            .call("schedule_get", serde_json::json!({"id": "will-work"}))
            .await
            .unwrap();
        assert_eq!(got["schedule"]["last_error"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn cron_expression_fires_repeatedly() {
        let shim = start_plugin(scan_config()).await;
        shim.call(
            "schedule_set",
            serde_json::json!({
                "id": "ticker",
                "cron": {"expr": "* * * * * *"},
                "event": {"payload": {"tick": 1}}
            }),
        )
        .await
        .unwrap();

        // Every-second expression against a 1s scan: expect ≥2 fires within
        // ~3s, then that the counter keeps advancing on the document.
        tokio::time::sleep(Duration::from_millis(3_000)).await;
        let count = shim
            .published()
            .await
            .into_iter()
            .filter(|(t, p)| t == "fired" && p["schedule_id"] == "ticker")
            .count();
        assert!(count >= 2, "expected repeated cron fires, got {count}");

        let got = shim
            .call("schedule_get", serde_json::json!({"id": "ticker"}))
            .await
            .unwrap();
        let fire_count = got["schedule"]["fire_count"].as_u64().unwrap();
        assert!(
            fire_count >= 2,
            "document counter must advance: {fire_count}"
        );
        assert_eq!(
            got["schedule"]["fired_once"], false,
            "cron never sets the one-shot flag"
        );
    }

    #[tokio::test]
    async fn delete_is_idempotent_and_publishes_changed() {
        let shim = start_plugin(no_scan_config()).await;
        shim.call(
            "schedule_set",
            serde_json::json!({"id": "bye", "once": {"at_ms": 9_999}, "event": {}}),
        )
        .await
        .unwrap();

        let first = shim
            .call("schedule_delete", serde_json::json!({"id": "bye"}))
            .await
            .unwrap();
        assert_eq!(first["deleted"], true);

        let second = shim
            .call("schedule_delete", serde_json::json!({"id": "bye"}))
            .await
            .unwrap();
        assert_eq!(second["deleted"], false);

        for _ in 0..120 {
            if shim.published().await.iter().any(|(t, p)| {
                t == "changed"
                    && p.get("op").and_then(Value::as_str) == Some("deleted")
                    && p.get("id").and_then(Value::as_str) == Some("bye")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            shim.published().await.iter().any(|(t, p)| {
                t == "changed"
                    && p.get("op").and_then(Value::as_str) == Some("deleted")
                    && p.get("id").and_then(Value::as_str) == Some("bye")
            }),
            "changed(deleted) event missing"
        );
    }

    #[tokio::test]
    async fn validation_errors_surface_as_action_errors() {
        let shim = start_plugin(no_scan_config()).await;

        let err = shim
            .call("schedule_set", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            err.contains("exactly one of params.once or params.cron"),
            "{err}"
        );

        let err = shim
            .call(
                "schedule_set",
                serde_json::json!({"cron": {"expr": "garbage"}, "event": {}}),
            )
            .await
            .unwrap_err();
        assert!(err.contains("invalid cron expression"), "{err}");

        let err = shim
            .call(
                "schedule_set",
                serde_json::json!({"id": "has space", "once": {"at_ms": 5}, "event": {}}),
            )
            .await
            .unwrap_err();
        assert!(err.contains("'_' and '-'"), "{err}");

        let err = shim
            .call("schedule_set", serde_json::json!({"once": {"at_ms": 5}}))
            .await
            .unwrap_err();
        assert!(
            err.contains("exactly one of params.event or params.action"),
            "{err}"
        );

        let err = shim
            .call("schedule_frobnicate", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }

    #[tokio::test]
    async fn disabled_schedule_never_fires() {
        let shim = start_plugin(scan_config()).await;
        let now = store::now_ms();
        shim.call(
            "schedule_set",
            serde_json::json!({
                "id": "sleeping",
                "enabled": false,
                "once": {"at_ms": now - 1_000},
                "event": {}
            }),
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(1_600)).await;
        let count = shim
            .published()
            .await
            .into_iter()
            .filter(|(t, p)| t == "fired" && p["schedule_id"] == "sleeping")
            .count();
        assert_eq!(count, 0, "disabled schedules must not fire");

        let got = shim
            .call("schedule_get", serde_json::json!({"id": "sleeping"}))
            .await
            .unwrap();
        assert_eq!(got["schedule"]["fired_once"], false);
        assert_eq!(got["schedule"]["fire_count"], 0);
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
