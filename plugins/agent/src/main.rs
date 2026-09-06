//! `agent` plugin binary — the multi-step goal loop over `ai`'s
//! `chat_completion` with kernel-routed tool dispatch.
//!
//! Serve-loop architecture (sync-client/calendar model): the loop task
//! exclusively owns the `VynkorClient` and is the single reader of the
//! connection, so no inbound frame is ever discarded. Goal handlers run as
//! spawned tasks that reach `database`/`ai`/catalogued tools through the
//! [`Rpc`] proxy channel; replies and fire-and-forget events flow back
//! through an outbound channel the loop drains. A goal run makes many
//! outbound round-trips, so without the proxy every mid-run user request
//! would be eaten by `send_action`'s discard-while-waiting.

use std::collections::HashMap;
use std::sync::Arc;

use agent_plugin::{
    command_ack_result, handle_action, CommandCall, Config, ProxyMsg, Rpc, RpcCall,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, EventPublish, KernelCommand,
    PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "agent";
const PLUGIN_VERSION: &str = "0.1.4";
const ACTIONS: [&str; 8] = [
    "goal_start",
    "goal_get",
    "goal_list",
    "goal_resume",
    "tools_list",
    "memory_forget",
    "memory_clear",
    "memory_list",
];

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec![
            "PERMISSION_STORAGE".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn is_device_target(action: &str) -> bool {
    action.starts_with("dev-") && action.contains('.')
}

fn route_target(action: &str) -> (String, String) {
    if is_device_target(action) {
        (action.to_string(), String::new())
    } else {
        ("kernel".to_string(), action.to_string())
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
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<ProxyMsg>(64);
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
                        // agent declares no event subscriptions; ack
                        // defensively so nothing unexpected is retried.
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
                    Some(envelope::Payload::KernelCommandAck(ack)) => {
                        if let Some((command, reply)) = pending.remove(&ack.command_id) {
                            let _ = reply.send(command_ack_result(
                                ack.status,
                                ack.data_json,
                                format!("{command} failed: {}", ack.error),
                            ));
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
            Some(msg) = rpc_rx.recv() => {
                seq += 1;
                match msg {
                    ProxyMsg::Action(call) => {
                        let RpcCall { action, params_json, timeout_ms, reply } = call;
                        let action_id = format!("rpc-{seq}");
                        pending.insert(action_id.clone(), (action.clone(), reply));
                        let (target, act) = route_target(&action);
                        let env = Envelope {
                            payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                                action_id,
                                action: act,
                                params_json,
                                timeout_ms,
                                streaming: false,
                                ..Default::default()
                            })),
                            ..Default::default()
                        };
                        let _ = client.send(&target, env).await;
                    }
                    ProxyMsg::Command(call) => {
                        let CommandCall { command_id, command, params_json, timeout_ms: _, reply } =
                            call;
                        pending.insert(command_id.clone(), (command.clone(), reply));
                        let env = Envelope {
                            payload: Some(envelope::Payload::KernelCommand(KernelCommand {
                                command_id,
                                command,
                                params_json,
                            })),
                            ..Default::default()
                        };
                        let _ = client.send("kernel", env).await;
                    }
                }
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
    use std::collections::{BTreeMap, HashMap as StdHashMap, VecDeque};
    use std::sync::OnceLock;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;
    use vynkor_sdk::proto::{
        ActionResponse as ProtoActionResponse, CommandStatus, EventPublishAck, EventPublishStatus,
        PluginRegisterAck,
    };

    /// Shared process-env setup for all tests in this binary. Tests run in
    /// parallel threads sharing one environment (`docs/PLUGIN_AUTHORING.md`
    /// §6): every var is set exactly once via this helper, to values that
    /// work for all tests simultaneously. The tools file marks `fs_read`
    /// confirmation-required so confirm-gate tests can share the setup.
    static ENV: OnceLock<()> = OnceLock::new();

    fn setup_env() {
        ENV.get_or_init(|| {
            std::env::set_var("AGENT_PLUGIN_AI_PROVIDER", "openai");
            std::env::set_var("AGENT_PLUGIN_AI_BASE_URL", "http://llm.test/v1");
            std::env::set_var("AGENT_PLUGIN_AI_MODEL", "test-model");
            std::env::set_var("AGENT_PLUGIN_AI_API_KEY_ENV", "TEST_LLM_KEY");
            std::env::set_var("AGENT_PLUGIN_ALLOWED_ACTIONS", "notify_send,fs_read");
            let dir = std::env::temp_dir().join(format!("vynkor-agent-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("tools.json");
            // Deliberately ONLY notify_send: fs_read then comes exclusively
            // from kernel manifest discovery, so tests prove both layers.
            std::fs::write(
                &path,
                serde_json::json!({"tools": [
                    {"name": "notify_send", "description": "file curated notification",
                     "parameters": {"type": "object"}}
                ]})
                .to_string(),
            )
            .unwrap();
            std::env::set_var("AGENT_PLUGIN_TOOLS_FILE", path.as_os_str());
        });
    }

    /// Fixture of what the kernel's `list_plugins`/`get_manifest` return:
    /// two registered plugins whose action_specs feed tool discovery.
    fn fixture_registry() -> Value {
        serde_json::json!({
            "notify": {
                "actions": ["notify_send"],
                "action_specs": [
                    {"name": "notify_send", "description": "kernel described notification",
                     "params_schema": "{\"type\":\"object\",\"properties\":{\"title\":{\"type\":\"string\"}}}",
                     "risk": "low", "requires_confirmation": false}
                ]
            },
            "filesystem": {
                "actions": ["fs_list", "fs_read", "fs_write"],
                "action_specs": [
                    {"name": "fs_list", "description": "List a directory",
                     "params_schema": "{\"type\":\"object\"}", "risk": "low",
                     "requires_confirmation": false},
                    {"name": "fs_read", "description": "Read a file",
                     "params_schema": "{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}}}",
                     "risk": "medium", "requires_confirmation": true}
                ]
            }
        })
    }

    type Published = Arc<Mutex<Vec<(String, Value)>>>;
    /// Kernel-routed calls the plugin fired as agent tools (non-db, non-ai).
    type Dispatched = Arc<Mutex<Vec<(String, Value)>>>;
    type AiRequests = Arc<Mutex<Vec<Value>>>;

    /// In-memory stand-in for the `database` plugin (same KV semantics the
    /// notes/calendar tests use).
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
        PushAiReply { content: String },
        PushAiReplyValue { reply: Value },
    }

    struct Shim {
        tx: mpsc::Sender<Cmd>,
        published: Published,
        dispatched: Dispatched,
        ai_requests: AiRequests,
    }

    impl Shim {
        async fn call_action(&self, action: &str, params: Value) -> Result<Value, String> {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.tx
                .send(Cmd::Call { action: action.into(), params, reply: reply_tx })
                .await
                .expect("shim loop died");
            tokio::time::timeout(Duration::from_secs(10), reply_rx)
                .await
                .expect("timed out waiting for plugin reply")
                .expect("shim dropped reply channel")
        }

        /// Call one of the plugin's own actions through the fake kernel.
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            self.call_action(action, params).await
        }

        async fn push_ai_reply(&self, content: &str) {
            self.tx
                .send(Cmd::PushAiReply { content: content.to_string() })
                .await
                .expect("shim loop died");
        }

        /// Script a full normalized `chat_completion` payload — e.g. with a
        /// native `tool_calls` array.
        async fn push_ai_reply_value(&self, reply: Value) {
            self.tx
                .send(Cmd::PushAiReplyValue { reply })
                .await
                .expect("shim loop died");
        }

        async fn published(&self) -> Vec<(String, Value)> {
            self.published.lock().await.clone()
        }

        async fn dispatched(&self) -> Vec<(String, Value)> {
            self.dispatched.lock().await.clone()
        }

        async fn ai_requests(&self) -> Vec<Value> {
            self.ai_requests.lock().await.clone()
        }
    }

    async fn start_plugin(config: Config) -> Shim {
        start_plugin_with(config, false).await
    }

    /// `commands_denied: true` simulates an older kernel where the discovery
    /// commands are unknown/denied — the plugin must fall back to its static
    /// catalog instead of failing.
    async fn start_plugin_with(config: Config, commands_denied: bool) -> Shim {
        setup_env();
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        tokio::spawn(async move {
            let _ = serve(plugin_client, config).await;
        });

        let (tx, rx) = mpsc::channel::<Cmd>(32);
        let shim = Shim {
            tx,
            published: Arc::new(Mutex::new(Vec::new())),
            dispatched: Arc::new(Mutex::new(Vec::new())),
            ai_requests: Arc::new(Mutex::new(Vec::new())),
        };
        tokio::spawn(run_shim(
            kernel_client,
            rx,
            shim.published.clone(),
            shim.dispatched.clone(),
            shim.ai_requests.clone(),
            commands_denied,
        ));
        shim
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_shim(
        mut kernel: VynkorClient,
        mut rx: mpsc::Receiver<Cmd>,
        published: Published,
        dispatched: Dispatched,
        ai_requests: AiRequests,
        commands_denied: bool,
    ) {
        let mut db = FakeDb::default();
        let mut ai_replies: VecDeque<Value> = VecDeque::new();
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
                    let _ = kernel.send("agent", Envelope {
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
                            let outcome = if req.action.starts_with("db_") {
                                db.handle(&req.action, params)
                            } else if req.action == "chat_completion" {
                                ai_requests.lock().await.push(params.clone());
                                match ai_replies.pop_front() {
                                    Some(reply) => Ok(reply),
                                    None => Err("no scripted ai reply".to_string()),
                                }
                            } else {
                                dispatched.lock().await.push((req.action.clone(), params));
                                Ok(serde_json::json!({"ok": true}))
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
                            let _ = kernel.send("agent", Envelope {
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
                            let _ = kernel.send("agent", Envelope {
                                payload: Some(envelope::Payload::EventPublishAck(EventPublishAck {
                                    event_id: format!("ev-{seq}"),
                                    status: EventPublishStatus::EventPublishOk as i32,
                                    error: String::new(),
                                })),
                                ..Default::default()
                            }).await;
                            seq += 1;
                        }
                        Some(envelope::Payload::KernelCommand(cmd)) => {
                            let params: Value = serde_json::from_slice(&cmd.params_json)
                                .unwrap_or(Value::Null);
                            let outcome: Result<Value, String> = if commands_denied {
                                Err(format!("unknown command: {}", cmd.command))
                            } else {
                                match cmd.command.as_str() {
                                    "list_plugins" => {
                                        let registry = fixture_registry();
                                        let plugins: Vec<Value> = registry
                                            .as_object()
                                            .unwrap()
                                            .iter()
                                            .map(|(slug, m)| {
                                                serde_json::json!({
                                                    "plugin_id": slug,
                                                    "state": "Running",
                                                    "actions": m["actions"],
                                                })
                                            })
                                            .collect();
                                        Ok(Value::Array(plugins))
                                    }
                                    "get_manifest" => {
                                        let slug =
                                            params.get("plugin_id").and_then(Value::as_str).unwrap_or_default();
                                        match fixture_registry().get(slug) {
                                            Some(m) => Ok(serde_json::json!({
                                                "plugin_id": slug,
                                                "action_specs": m["action_specs"],
                                            })),
                                            None => Err(format!("plugin not registered: {slug}")),
                                        }
                                    }
                                    other => Err(format!("unknown command: {other}")),
                                }
                            };
                            let ack = match outcome {
                                Ok(v) => vynkor_sdk::proto::KernelCommandAck {
                                    command_id: cmd.command_id.clone(),
                                    status: CommandStatus::CommandOk as i32,
                                    data_json: serde_json::to_vec(&v).unwrap(),
                                    error: String::new(),
                                },
                                Err(e) => vynkor_sdk::proto::KernelCommandAck {
                                    command_id: cmd.command_id.clone(),
                                    status: CommandStatus::CommandUnknown as i32,
                                    data_json: Vec::new(),
                                    error: e,
                                },
                            };
                            let _ = kernel.send("agent", Envelope {
                                payload: Some(envelope::Payload::KernelCommandAck(ack)),
                                ..Default::default()
                            }).await;
                        }
                        Some(envelope::Payload::Ping(ping)) => {
                            let _ = kernel.send("agent", Envelope {
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
                        Some(Cmd::PushAiReply { content }) => {
                            ai_replies.push_back(serde_json::json!({"content": content}));
                        }
                        Some(Cmd::PushAiReplyValue { reply }) => {
                            ai_replies.push_back(reply);
                        }
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
                            let _ = kernel.send("agent", env).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

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

    #[tokio::test]
    async fn tool_then_final_completes_and_persists() {
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply(r#"{"tool": "notify_send", "params": {"title": "hi"}}"#).await;
        shim.push_ai_reply("All done — notified.").await;

        let res = shim
            .call("goal_start", serde_json::json!({"goal": "notify me", "title": "demo"}))
            .await
            .unwrap();
        assert_eq!(res["status"], "completed");
        assert_eq!(res["final_answer"], "All done — notified.");
        let id = res["id"].as_str().unwrap().to_string();

        let dispatched = shim.dispatched().await;
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].0, "notify_send");
        assert_eq!(dispatched[0].1["title"], "hi");

        let got = shim.call("goal_get", serde_json::json!({"id": id})).await.unwrap();
        assert_eq!(got["found"], true);
        assert_eq!(got["goal"]["title"], "demo");
        let kinds: Vec<&str> = got["goal"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["tool_ok", "final"]);

        let changed = wait_for_published(&shim, |(t, p)| {
            t == "changed" && p.get("id").and_then(Value::as_str) == Some(id.as_str())
        })
        .await
        .expect("changed event missing");
        assert_eq!(changed.1["op"], "completed");

        // The model saw the catalog and the tool result.
        let reqs = shim.ai_requests().await;
        assert_eq!(reqs.len(), 2);
        assert!(reqs[0]["messages"][0]["content"].as_str().unwrap().contains("notify_send"));
        let second_msgs = reqs[1]["messages"].as_array().unwrap();
        assert!(
            second_msgs
                .iter()
                .any(|m| m["content"].as_str().unwrap_or_default().contains("[TOOL RESULT")),
            "expected the tool result fed back into the second call"
        );
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_back_never_dispatched() {
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply(r#"{"tool": "db_delete_all", "params": {}}"#).await;
        shim.push_ai_reply("Cannot do that.").await;

        let res = shim.call("goal_start", serde_json::json!({"goal": "nuke db"})).await.unwrap();
        assert_eq!(res["status"], "completed");
        assert_eq!(res["final_answer"], "Cannot do that.");
        assert!(shim.dispatched().await.is_empty());

        let kinds: Vec<&str> = res["steps"].as_array().unwrap()
            .iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["unknown_tool", "final"]);
    }

    #[tokio::test]
    async fn confirmation_tool_halts_then_resume_approves() {
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply(r#"{"tool": "fs_read", "params": {"path": "/tmp/x"}}"#).await;
        shim.push_ai_reply("Read it.").await;

        let res = shim.call(
            "goal_start",
            serde_json::json!({"goal": "read /tmp/x"}),
        ).await.unwrap();
        assert_eq!(res["status"], "needs_confirmation");
        assert_eq!(res["pending_tool"], "fs_read");
        assert!(shim.dispatched().await.is_empty(), "must not dispatch before approval");
        let id = res["id"].as_str().unwrap().to_string();

        let resumed = shim
            .call("goal_resume", serde_json::json!({"id": id, "approve": true}))
            .await
            .unwrap();
        assert_eq!(resumed["status"], "completed");
        assert_eq!(resumed["final_answer"], "Read it.");

        let dispatched = shim.dispatched().await;
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].0, "fs_read");
        assert_eq!(dispatched[0].1["path"], "/tmp/x");
    }

    #[tokio::test]
    async fn decline_marks_goal_declined_and_blocks_further_resume() {
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply(r#"{"tool": "fs_read", "params": {"path": "/tmp/y"}}"#).await;

        let res = shim.call("goal_start", serde_json::json!({"goal": "read /tmp/y"})).await.unwrap();
        assert_eq!(res["status"], "needs_confirmation");
        let id = res["id"].as_str().unwrap().to_string();

        let declined = shim
            .call("goal_resume", serde_json::json!({"id": id, "approve": false}))
            .await
            .unwrap();
        assert_eq!(declined["status"], "declined");
        assert!(declined["final_answer"].as_str().unwrap().contains("declined"));
        assert!(shim.dispatched().await.is_empty());

        let err = shim
            .call("goal_resume", serde_json::json!({"id": id, "approve": true}))
            .await
            .unwrap_err();
        assert!(err.contains("not awaiting confirmation"), "{err}");
    }

    #[tokio::test]
    async fn native_tool_call_dispatches_structured_reply() {
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply_value(serde_json::json!({
            "content": "",
            "tool_calls": [
                {"id": "c1", "name": "notify_send", "arguments_json": "{\"title\": \"hi\"}"}
            ]
        }))
        .await;
        shim.push_ai_reply("Done.").await;

        let res = shim.call("goal_start", serde_json::json!({"goal": "notify"})).await.unwrap();
        assert_eq!(res["status"], "completed", "{res}");
        assert_eq!(res["final_answer"], "Done.");
        let dispatched = shim.dispatched().await;
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].0, "notify_send");
        assert_eq!(dispatched[0].1["title"], "hi");
    }

    #[tokio::test]
    async fn chat_completion_receives_native_tools_param() {
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply("All done.").await;

        let res = shim.call("goal_start", serde_json::json!({"goal": "anything"})).await.unwrap();
        assert_eq!(res["status"], "completed");

        let requests = shim.ai_requests().await;
        assert_eq!(requests.len(), 1);
        let tools = requests[0]["tools"].as_array().expect("tools param missing");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["notify_send", "fs_read"]);
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[tokio::test]
    async fn max_steps_bounds_the_loop() {
        let shim = start_plugin(Config::default()).await;
        // Every scripted reply is another allowed tool call; budget of 1
        // stops the loop after the first dispatch.
        shim.push_ai_reply(r#"{"tool": "notify_send", "params": {"n": 1}}"#).await;
        shim.push_ai_reply(r#"{"tool": "notify_send", "params": {"n": 2}}"#).await;

        let res = shim
            .call("goal_start", serde_json::json!({"goal": "spam", "max_steps": 1}))
            .await
            .unwrap();
        assert_eq!(res["status"], "max_steps_reached");
        assert_eq!(shim.dispatched().await.len(), 1);
        // The second scripted reply was never consumed.
        assert_eq!(shim.ai_requests().await.len(), 1);
    }

    #[tokio::test]
    async fn llm_failure_lands_in_status_error() {
        let shim = start_plugin(Config::default()).await;
        // No scripted replies → chat_completion errors inside the loop.

        let res = shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        assert_eq!(res["status"], "error");
        assert!(res["error"].as_str().unwrap().contains("chat_completion"), "{res}");
    }

    #[tokio::test]
    async fn goal_list_returns_newest_first_and_missing_get_found_false() {
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply("one").await;
        shim.push_ai_reply("two").await;

        shim.call("goal_start", serde_json::json!({"goal": "first"})).await.unwrap();
        shim.call("goal_start", serde_json::json!({"goal": "second"})).await.unwrap();

        let list = shim.call("goal_list", serde_json::json!({})).await.unwrap();
        assert_eq!(list["total"], 2);
        let goals: Vec<&str> = list["goals"].as_array().unwrap()
            .iter().map(|g| g["goal"].as_str().unwrap()).collect();
        assert_eq!(goals.first(), Some(&"second"));

        let missing = shim.call("goal_get", serde_json::json!({"id": "999"})).await.unwrap();
        assert_eq!(missing["found"], false);

        let err = shim
            .call("goal_resume", serde_json::json!({"id": "999", "approve": true}))
            .await
            .unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn tools_list_shows_catalog_and_flags() {
        let shim = start_plugin(Config::default()).await;
        let res = shim.call("tools_list", serde_json::json!({})).await.unwrap();
        assert_eq!(res["tools_file_set"], true);
        let names: Vec<&str> = res["tools"].as_array().unwrap()
            .iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["notify_send", "fs_read"]);

        // notify_send comes from the operator tools file…
        let notify = res["tools"].as_array().unwrap().iter()
            .find(|t| t["name"] == "notify_send").unwrap();
        assert_eq!(notify["source"], "file");
        assert_eq!(notify["description"], "file curated notification");

        // …fs_read has no file entry, so discovery filled it from the
        // registered manifest — schema, risk and confirmation flag included.
        let fs = res["tools"].as_array().unwrap().iter()
            .find(|t| t["name"] == "fs_read").unwrap();
        assert_eq!(fs["source"], "kernel");
        assert_eq!(fs["description"], "Read a file");
        assert_eq!(fs["risk"], "medium");
        assert_eq!(fs["requires_confirmation"], true);
        assert_eq!(
            fs["parameters"]["properties"]["path"]["type"],
            "string",
            "params_schema decoded into a JSON object for the prompt"
        );
    }

    #[tokio::test]
    async fn confirmation_flag_from_kernel_manifest_halts_the_loop() {
        // fs_read's requires_confirmation arrives via get_manifest, not the
        // tools file — proving kernel-sourced gating end to end.
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply(r#"{"tool": "fs_read", "params": {"path": "/tmp/k"}}"#).await;

        let res = shim.call("goal_start", serde_json::json!({"goal": "read"})).await.unwrap();
        assert_eq!(res["status"], "needs_confirmation");
        assert_eq!(res["pending_tool"], "fs_read");
        assert!(shim.dispatched().await.is_empty());
    }

    #[tokio::test]
    async fn denied_discovery_falls_back_to_static_catalog() {
        // An older kernel: commands unknown → fs_read stays Minimal
        // (no confirmation info), goals keep working, nothing fails.
        let shim = start_plugin_with(Config::default(), true).await;
        shim.push_ai_reply(r#"{"tool": "fs_read", "params": {"path": "/tmp/z"}}"#).await;
        shim.push_ai_reply("done").await;

        let res = shim.call("goal_start", serde_json::json!({"goal": "read"})).await.unwrap();
        assert_eq!(res["status"], "completed", "fallback must not halt on Minimal specs");

        let dispatched = shim.dispatched().await;
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].0, "fs_read");

        let tools = shim.call("tools_list", serde_json::json!({})).await.unwrap();
        let fs = tools["tools"].as_array().unwrap().iter()
            .find(|t| t["name"] == "fs_read").unwrap();
        assert_eq!(fs["source"], "minimal");
        assert_eq!(fs["requires_confirmation"], false);
    }

    #[tokio::test]
    async fn validation_and_unknown_actions_surface_as_errors() {
        let shim = start_plugin(Config::default()).await;
        let err = shim.call("goal_start", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("goal"), "{err}");
        let err = shim
            .call("goal_start", serde_json::json!({"goal": "g", "max_steps": 99}))
            .await
            .unwrap_err();
        assert!(err.contains("max_steps"), "{err}");
        let err = shim.call("frobnicate", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }
}
