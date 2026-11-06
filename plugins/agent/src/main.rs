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
const PLUGIN_VERSION: &str = "0.1.6";
const ACTIONS: [&str; 10] = [
    "goal_start",
    "goal_get",
    "goal_list",
    "goal_resume",
    "tools_list",
    "memory_forget",
    "memory_clear",
    "memory_list",
    "memory_forget_session",
    "goal_delete",
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
                            match handle_action(rpc, &config, &req.caller_plugin_id, &req.action, &req.params_json)
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

    /// Serializes the fake-kernel e2e tests. They share one process env
    /// (`setup_env` sets every var once), and the memory test mutates
    /// `AGENT_PLUGIN_MEMORY`, so goal loops must never run concurrently.
    static TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn setup_env() {
        ENV.get_or_init(|| {
            std::env::set_var("AGENT_PLUGIN_AI_PROVIDER", "openai");
            std::env::set_var("AGENT_PLUGIN_AI_BASE_URL", "http://llm.test/v1");
            std::env::set_var("AGENT_PLUGIN_AI_MODEL", "test-model");
            std::env::set_var("AGENT_PLUGIN_AI_API_KEY_ENV", "TEST_LLM_KEY");
            std::env::set_var("AGENT_PLUGIN_ALLOWED_ACTIONS", "notify_send,fs_read,secret_get,ping_a,ping_b");
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
            },
            "secrets": {
                "actions": ["secret_get"],
                "action_specs": [
                    {"name": "secret_get", "description": "Read a secret",
                     "params_schema": "{\"type\":\"object\"}", "risk": "low",
                     "requires_confirmation": false}
                ]
            }
        })
    }

    type Published = Arc<Mutex<Vec<(String, Value)>>>;
    /// Kernel-routed calls the plugin fired as agent tools (non-db, non-ai).
    type Dispatched = Arc<Mutex<Vec<(String, Value)>>>;
    type AiRequests = Arc<Mutex<Vec<Value>>>;
    /// Per-collection facts in the fake `vector-db`: written by `vec_upsert`,
    /// read by `vec_query`, seeded directly by tests.
    type VecStore = Arc<Mutex<StdHashMap<String, Vec<Value>>>>;
    /// Shared handle onto the fake `database` plugin's KV store — lets
    /// tests seed keys (e.g. `memory:index`) the same way `vec_store` lets
    /// them seed fake vector-db collections.
    type DbStore = Arc<Mutex<FakeDb>>;
    type ChatsStore = Arc<Mutex<FakeChats>>;

    /// How the fake kernel answers `chat_*` actions.
    #[derive(Default, Clone, PartialEq)]
    enum ChatsMode {
        /// Plugin not installed: `action not found`.
        #[default]
        Missing,
        /// Installed and healthy.
        Normal,
        /// Installed but never answers (exercises the call timeout).
        Silent,
    }

    /// In-memory stand-in for the `chats` plugin: scripted reads, recorded calls.
    #[derive(Default)]
    struct FakeChats {
        mode: ChatsMode,
        session_id: String,
        created: bool,
        turns: Vec<Value>,
        calls: Vec<(String, Value)>,
        /// `chat_recall` answer (`hits`), scripted by tests.
        recall_hits: Vec<Value>,
        /// `chat_recall` never answers (a lookup slower than any budget).
        recall_silent: bool,
    }

    const CHATS_ACTIONS: [&str; 4] =
        ["chat_session_resolve", "chat_session_read", "chat_turn_append", "chat_recall"];

    impl FakeChats {
        fn handle(&mut self, action: &str, params: Value) -> Option<Result<Value, String>> {
            self.calls.push((action.to_string(), params));
            if action == "chat_recall" && self.mode == ChatsMode::Normal && self.recall_silent {
                return None;
            }
            match self.mode {
                ChatsMode::Missing => Some(Err("action not found".to_string())),
                ChatsMode::Silent => None,
                ChatsMode::Normal => Some(Ok(match action {
                    "chat_session_resolve" => {
                        serde_json::json!({"session_id": self.session_id, "created": self.created})
                    }
                    "chat_session_read" => serde_json::json!({
                        "session": {"id": self.session_id}, "turns": self.turns
                    }),
                    "chat_recall" => serde_json::json!({"hits": self.recall_hits}),
                    _ => serde_json::json!({"turn_id": "T1", "seq": 1, "rev": 1}),
                })),
            }
        }
    }

    /// In-memory stand-in for the `database` plugin (same KV semantics the
    /// notes/calendar tests use).
    #[derive(Default)]
    struct FakeDb {
        kv: BTreeMap<String, Value>,
        /// Fail exactly the Nth `db_set` of a `goal:*` key (1-based), once.
        fail_goal_set_at: Option<usize>,
        goal_sets: usize,
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
                    if k.starts_with("goal:") {
                        self.goal_sets += 1;
                        if self.fail_goal_set_at == Some(self.goal_sets) {
                            return Err("injected db_set failure".to_string());
                        }
                    }
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
                // The agent issues exactly one `db_query`: goal_list's
                // projected newest-first listing (store.rs::list_summaries).
                // This fake does not interpret SQL — it mirrors that query's
                // semantics from its bound parameters. Assert the shape being
                // emulated rather than matching any query blindly, so a second,
                // different query fails loudly here instead of silently
                // receiving goal-shaped rows.
                //
                // Not emulated: expiry filtering (this map has no TTL column),
                // and `json_array_length` of a missing `steps` array yields 0
                // here where SQLite would yield NULL.
                "db_query" => {
                    let sql = params.get("sql").and_then(Value::as_str).unwrap_or_default();
                    assert!(
                        sql.contains("json_array_length(value, '$.steps')")
                            && sql.contains("from kv"),
                        "FakeDb only emulates goal_list's projection query, got: {sql}"
                    );
                    let bound =
                        params.get("params").and_then(Value::as_array).cloned().unwrap_or_default();
                    let pattern = bound.first().and_then(Value::as_str).unwrap_or("");
                    let prefix = pattern.trim_end_matches('%');
                    let limit =
                        bound.get(3).and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;

                    // Numeric suffix order, not lexical: goal:9 is newer than
                    // goal:10 would be under a plain string sort.
                    fn key_num(k: &str) -> i64 {
                        k.rsplit(':').next().and_then(|s| s.parse().ok()).unwrap_or(0)
                    }
                    let mut matched: Vec<(&String, &Value)> =
                        self.kv.iter().filter(|(k, _)| k.starts_with(prefix)).collect();
                    matched.sort_by_key(|(k, _)| std::cmp::Reverse(key_num(k)));

                    let rows: Vec<Value> = matched
                        .into_iter()
                        .take(limit)
                        .map(|(_, doc)| {
                            let f = |name: &str| doc.get(name).cloned().unwrap_or(Value::Null);
                            serde_json::json!({
                                "id": f("id"),
                                "title": f("title"),
                                "goal": f("goal"),
                                "status": f("status"),
                                "final_answer": f("final_answer"),
                                "error": f("error"),
                                "pending_tool": f("pending_tool"),
                                "step_count": doc
                                    .get("steps")
                                    .and_then(Value::as_array)
                                    .map_or(0, Vec::len),
                                "max_steps": f("max_steps"),
                                "created_at_ms": f("created_at_ms"),
                                "updated_at_ms": f("updated_at_ms"),
                            })
                        })
                        .collect();
                    Ok(serde_json::json!({ "rows": rows }))
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
        vec_store: VecStore,
        db: DbStore,
        chats: ChatsStore,
        _serial: tokio::sync::MutexGuard<'static, ()>,
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

        /// Script the NEXT `chat_completion` call to fail, as if the
        /// provider rejected the request (e.g. a model without native
        /// tool-use support rejecting the `tools` param). Used to exercise
        /// the engine's native-tools degrade path deterministically.
        async fn push_ai_failure(&self, message: &str) {
            self.tx
                .send(Cmd::PushAiReplyValue {
                    reply: serde_json::json!({"__fail__": message}),
                })
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

        async fn seed_fact(&self, collection: &str, id: &str, fact: &str) {
            self.vec_store
                .lock()
                .await
                .entry(collection.to_string())
                .or_default()
                .push(serde_json::json!({"id": id, "text": fact, "metadata": {"fact": fact}}));
        }

        /// Append an entry to the fake `memory:index` key, mirroring what
        /// `remember()` does in production so a seeded fact is discoverable
        /// through the same index `recall`'s empty-index short-circuit
        /// checks (see `memory.rs`).
        async fn seed_index(&self, id: &str) {
            let mut db = self.db.lock().await;
            let key = agent_plugin::memory::INDEX_KEY.to_string();
            let mut entries: Vec<Value> = db
                .kv
                .get(&key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            entries.push(serde_json::json!({"id": id, "ts": 0}));
            db.kv.insert(key, Value::Array(entries));
        }

        /// Install a healthy fake `chats` returning this session and history.
        async fn chats_normal(&self, session_id: &str, created: bool, turns: Vec<Value>) {
            let mut c = self.chats.lock().await;
            c.mode = ChatsMode::Normal;
            c.session_id = session_id.to_string();
            c.created = created;
            c.turns = turns;
        }

        async fn chats_mode(&self, mode: ChatsMode) {
            self.chats.lock().await.mode = mode;
        }

        /// `chat_*` calls the plugin made, in order, optionally one action only.
        async fn chats_calls(&self, action: &str) -> Vec<Value> {
            self.chats
                .lock()
                .await
                .calls
                .iter()
                .filter(|(a, _)| a == action)
                .map(|(_, p)| p.clone())
                .collect()
        }

        /// Make `vec_embed` answer with this model (`"fake"` = the placeholder).
        async fn embed_model(&self, model: &str) {
            self.seed_fact("__embed__", model, "").await;
        }

        /// Journal of `vec_embed` / `vec_query` calls (params), in order.
        async fn vec_calls(&self, action: &str) -> Vec<Value> {
            self.vec_facts("__vec_log__")
                .await
                .into_iter()
                .filter(|e| e["action"] == action)
                .map(|e| e["params"].clone())
                .collect()
        }

        async fn chats_recall_hits(&self, hits: Vec<Value>) {
            self.chats.lock().await.recall_hits = hits;
        }

        async fn vec_facts(&self, collection: &str) -> Vec<Value> {
            self.vec_store
                .lock()
                .await
                .get(collection)
                .cloned()
                .unwrap_or_default()
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
        let vec_store: VecStore = Arc::new(Mutex::new(StdHashMap::new()));
        let db: DbStore = Arc::new(Mutex::new(FakeDb::default()));
        let shim = Shim {
            tx,
            published: Arc::new(Mutex::new(Vec::new())),
            dispatched: Arc::new(Mutex::new(Vec::new())),
            ai_requests: Arc::new(Mutex::new(Vec::new())),
            vec_store: vec_store.clone(),
            db: db.clone(),
            chats: Arc::new(Mutex::new(FakeChats::default())),
            _serial: TEST_SERIAL.lock().await,
        };
        tokio::spawn(run_shim(
            kernel_client,
            rx,
            shim.published.clone(),
            shim.dispatched.clone(),
            shim.ai_requests.clone(),
            vec_store,
            db,
            shim.chats.clone(),
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
        vec_store: VecStore,
        db: DbStore,
        chats: ChatsStore,
        commands_denied: bool,
    ) {
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
                                db.lock().await.handle(&req.action, params)
                            } else if req.action == "vec_upsert" {
                                handle_vec_upsert(&vec_store, params).await
                            } else if req.action == "vec_delete" {
                                handle_vec_delete(&vec_store, params).await
                            } else if req.action == "vec_embed" {
                                handle_vec_embed(&vec_store, params).await
                            } else if req.action == "vec_query" {
                                handle_vec_query(&vec_store, params).await
                            } else if CHATS_ACTIONS.contains(&req.action.as_str()) {
                                match chats.lock().await.handle(&req.action, params) {
                                    Some(r) => r,
                                    None => continue, // Silent: never answer
                                }
                            } else if req.action == "chat_completion" {
                                ai_requests.lock().await.push(params.clone());
                                match ai_replies.pop_front() {
                                    Some(reply) => match reply.get("__fail__").and_then(Value::as_str) {
                                        Some(msg) => Err(msg.to_string()),
                                        None => Ok(reply),
                                    },
                                    None => Err("no scripted ai reply".to_string()),
                                }
                            } else {
                                let action_is_secret = req.action == "secret_get";
                                let big = params.get("big").and_then(Value::as_bool) == Some(true);
                                let envfail = req.action == "fs_read"
                                    && params.get("path").and_then(Value::as_str) == Some("/tmp/envfail");
                                dispatched.lock().await.push((req.action.clone(), params));
                                if envfail {
                                    Err("read failed:\nDB_PASSWORD=hunter2\nAPI_KEY=abc".to_string())
                                } else if action_is_secret {
                                    Ok(serde_json::json!({"secret": "hunter2", "note": "hunter2"}))
                                } else if big {
                                    Ok(serde_json::json!({"blob": "x".repeat(1_000_000)}))
                                } else {
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

    fn turn(seq: i64, q: &str, a: &str, status: &str) -> Value {
        serde_json::json!({"id": format!("T{seq}"), "seq": seq, "user_text": q,
                           "answer_text": a, "status": status, "tool_trace": null})
    }

    #[tokio::test]
    async fn goal_start_resolves_session_and_returns_it() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply("fine").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "hello"})).await.unwrap();
        assert_eq!(res["status"], "completed");
        assert_eq!(res["session_id"], "S1");
        let calls = shim.chats_calls("chat_session_resolve").await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["device"], "host");
        assert_eq!(calls[0]["kind"], "voice");
        assert_eq!(calls[0]["source"], "tester");
        assert!(calls[0].get("session_id").is_none(), "no session_id sent unless given");
        assert!(shim.chats_calls("chat_session_read").await.is_empty(), "new session: nothing to read");
        let got = shim.call("goal_get", serde_json::json!({"id": res["id"]})).await.unwrap();
        assert_eq!(got["goal"]["session_id"], "S1");
    }

    #[tokio::test]
    async fn history_turns_precede_goal_in_transcript() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal(
            "S1",
            false,
            vec![
                turn(1, "какая погода", "+5", "ok"),
                turn(2, "сломанный вопрос", "boom", "failed"),
            ],
        )
        .await;
        shim.push_ai_reply("завтра +7").await;
        shim.call("goal_start", serde_json::json!({"goal": "а завтра?", "session_id": "S1"}))
            .await
            .unwrap();
        let read = shim.chats_calls("chat_session_read").await;
        assert_eq!(read[0], serde_json::json!({"id": "S1", "last": 6}));
        let msgs = shim.ai_requests().await[0]["messages"].as_array().unwrap().clone();
        let pos = |role: &str, text: &str| {
            msgs.iter().position(|m| m["role"] == role && m["content"] == text)
        };
        // First question is folded into the role message; the answer and the goal follow.
        assert!(msgs[0]["content"].as_str().unwrap().ends_with("какая погода"));
        let a = pos("assistant", "+5").expect("history answer");
        let g = pos("user", "а завтра?").expect("goal");
        assert!(0 < a && a < g, "order: instructions+question, answer, goal");
        assert!(msgs.iter().all(|m| !m["content"].as_str().unwrap().contains("сломанный")),
            "failed turns stay out of the prompt");
        assert!(shim.chats_calls("chat_session_resolve").await[0]["session_id"] == "S1");
    }

    #[tokio::test]
    async fn history_transcript_alternates_roles_for_anthropic() {
        // `ai` forwards messages as is (no merging of same-role neighbours),
        // so the agent must send an alternating transcript.
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal(
            "S1",
            false,
            vec![turn(1, "q1", "a1", "ok"), turn(2, "q2", "a2", "ok"), turn(3, "q3", "a3", "ok")],
        )
        .await;
        shim.push_ai_reply("ok").await;
        shim.call("goal_start", serde_json::json!({"goal": "now", "provider": "anthropic"}))
            .await
            .unwrap();
        let reqs = shim.ai_requests().await;
        assert_eq!(reqs[0]["provider"], "anthropic");
        let roles: Vec<&str> = reqs[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant", "user", "assistant", "user"]);
    }

    #[tokio::test]
    async fn follow_up_keeps_tools_from_history() {
        let tool_names = |req: &Value| -> Vec<String> {
            req["tools"].as_array().unwrap().iter()
                .map(|t| t["name"].as_str().unwrap().to_string()).collect()
        };
        // Bare follow-up: the fs_* tool is filtered out of what the model sees.
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", false, vec![turn(1, "какая погода", "+5", "ok")]).await;
        shim.push_ai_reply("ok").await;
        shim.call("goal_start", serde_json::json!({"goal": "а завтра?"})).await.unwrap();
        assert!(!tool_names(&shim.ai_requests().await[0]).contains(&"fs_read".to_string()));
        drop(shim);

        // Same follow-up after a file question keeps it.
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", false, vec![turn(1, "прочитай файл notes.txt", "готово", "ok")]).await;
        shim.push_ai_reply("ok").await;
        shim.call("goal_start", serde_json::json!({"goal": "а завтра?"})).await.unwrap();
        assert!(tool_names(&shim.ai_requests().await[0]).contains(&"fs_read".to_string()));
    }

    #[tokio::test]
    async fn history_skips_turns_with_empty_text() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal(
            "S1",
            false,
            vec![
                turn(1, "empty answer", "  ", "ok"),
                turn(2, "", "empty question", "ok"),
                turn(3, "real q", "real a", "ok"),
            ],
        )
        .await;
        shim.push_ai_reply("ok").await;
        shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        let req = shim.ai_requests().await[0].to_string();
        assert!(req.contains("real q") && req.contains("real a"));
        assert!(!req.contains("empty answer") && !req.contains("empty question"), "{req}");
    }

    #[tokio::test]
    async fn history_wins_over_caller_context() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", false, vec![turn(1, "q1", "a1", "ok")]).await;
        shim.push_ai_reply("ok").await;
        shim.call("goal_start", serde_json::json!({"goal": "g", "context": "OLD-CONTEXT"}))
            .await
            .unwrap();
        let reqs = shim.ai_requests().await;
        assert!(!reqs[0].to_string().contains("OLD-CONTEXT"), "context must not reach the prompt");
    }

    #[tokio::test]
    async fn caller_context_still_used_when_history_is_empty() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", false, vec![]).await;
        shim.push_ai_reply("ok").await;
        shim.call("goal_start", serde_json::json!({"goal": "g", "context": "OLD-CONTEXT"}))
            .await
            .unwrap();
        assert!(shim.ai_requests().await[0].to_string().contains("OLD-CONTEXT"));
    }

    #[tokio::test]
    async fn missing_chats_plugin_keeps_goal_working() {
        let shim = start_plugin(Config::default()).await; // chats Missing by default
        shim.push_ai_reply("one").await;
        shim.push_ai_reply("two").await;
        for _ in 0..2 {
            let res = shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
            assert_eq!(res["status"], "completed");
            assert!(res["session_id"].is_null(), "{res}");
        }
        assert_eq!(shim.chats_calls("chat_session_resolve").await.len(), 2);
        assert!(shim.chats_calls("chat_session_read").await.is_empty());
    }

    #[tokio::test]
    async fn slow_chats_is_bounded_by_timeout() {
        let cfg = Config { chats_timeout_ms: 200, ..Config::default() };
        let shim = start_plugin(cfg).await;
        shim.chats_mode(ChatsMode::Silent).await;
        shim.push_ai_reply("done").await;
        let t0 = std::time::Instant::now();
        let res = shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        assert!(t0.elapsed() < Duration::from_secs(2), "took {:?}", t0.elapsed());
        assert_eq!(res["status"], "completed");
        assert!(res["session_id"].is_null());
    }

    #[tokio::test]
    async fn history_turns_zero_skips_read() {
        let cfg = Config { history_turns: 0, ..Config::default() };
        let shim = start_plugin(cfg).await;
        shim.chats_normal("S1", false, vec![turn(1, "q1", "a1", "ok")]).await;
        shim.push_ai_reply("ok").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        assert_eq!(res["session_id"], "S1");
        assert!(shim.chats_calls("chat_session_read").await.is_empty());
        assert!(!shim.ai_requests().await[0].to_string().contains("q1"));
    }

    /// `chat_turn_append` runs in the background after the reply: poll for it.
    async fn wait_for_chats_calls(shim: &Shim, action: &str, n: usize) -> Vec<Value> {
        for _ in 0..120 {
            let calls = shim.chats_calls(action).await;
            if calls.len() >= n {
                return calls;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        shim.chats_calls(action).await
    }

    #[tokio::test]
    async fn completed_goal_appends_turn_with_trace() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply(r#"{"tool": "notify_send", "params": {"title": "hi"}}"#).await;
        shim.push_ai_reply("notified").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "notify me"})).await.unwrap();
        let appended = wait_for_chats_calls(&shim, "chat_turn_append", 1).await;
        assert_eq!(appended.len(), 1);
        let a = &appended[0];
        assert_eq!(a["session_id"], "S1");
        assert_eq!(a["user_text"], "notify me");
        assert_eq!(a["answer_text"], "notified");
        assert_eq!(a["status"], "ok");
        assert_eq!(a["goal_id"], res["id"]);
        let t = &a["tool_trace"][0];
        assert_eq!(t["tool"], "notify_send");
        assert_eq!(t["args"], r#"{"title":"hi"}"#);
        assert!(t["result"].as_str().unwrap().contains("ok"));
        assert!(t["ms"].is_u64(), "{t}");
        assert!(t.get("error").is_none(), "no error key on success: {t}");
        // Only contract keys cross the wire (chats rejects unknown fields).
        let mut keys: Vec<&String> = a.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["answer_text", "goal_id", "session_id", "status", "tool_trace", "user_text"]);
        let mut tkeys: Vec<&String> = t.as_object().unwrap().keys().collect();
        tkeys.sort();
        assert_eq!(tkeys, ["args", "ms", "result", "tool"]);
        // The doc keeps the same trace.
        let got = shim.call("goal_get", serde_json::json!({"id": res["id"]})).await.unwrap();
        assert_eq!(got["goal"]["tool_trace"][0]["tool"], "notify_send");
    }

    #[tokio::test]
    async fn huge_tool_result_is_truncated_in_trace() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply(r#"{"tool": "notify_send", "params": {"big": true}}"#).await;
        shim.push_ai_reply("done").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "big"})).await.unwrap();
        assert_eq!(res["status"], "completed");
        let a = &wait_for_chats_calls(&shim, "chat_turn_append", 1).await[0];
        let result = a["tool_trace"][0]["result"].as_str().unwrap();
        assert_eq!(result.chars().count(), 201, "200 chars + ellipsis");
        let got = shim.call("goal_get", serde_json::json!({"id": res["id"]})).await.unwrap();
        let stored = got["goal"]["tool_trace"][0]["result"].as_str().unwrap();
        assert!(stored.chars().count() <= 201);
    }

    #[tokio::test]
    async fn secret_tool_trace_is_redacted() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        // secret_get must be a known tool: allow it via the kernel manifest fixture.
        shim.push_ai_reply(r#"{"tool": "secret_get", "params": {"name": "db_pass"}}"#).await;
        shim.push_ai_reply("fetched").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "get it"})).await.unwrap();
        assert_eq!(shim.dispatched().await[0].0, "secret_get", "tool must really run");
        let a = &wait_for_chats_calls(&shim, "chat_turn_append", 1).await[0];
        assert!(!a.to_string().contains("hunter2"), "secret leaked to chats: {a}");
        assert_eq!(a["tool_trace"][0]["result"], "[redacted]");
        assert_eq!(a["tool_trace"][0]["args"], r#"{"name":"[redacted]"}"#);
        let got = shim.call("goal_get", serde_json::json!({"id": res["id"]})).await.unwrap();
        assert!(!got["goal"]["tool_trace"].to_string().contains("hunter2"));
    }

    #[tokio::test]
    async fn fs_error_with_env_lines_never_reaches_chats() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply(r#"{"tool": "fs_read", "params": {"path": "/tmp/envfail"}}"#).await;
        shim.push_ai_reply("could not read").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        shim.call("goal_resume", serde_json::json!({"id": res["id"], "approve": true}))
            .await
            .unwrap();
        let a = &wait_for_chats_calls(&shim, "chat_turn_append", 1).await[0];
        assert!(!a.to_string().contains("hunter2"), "leak: {a}");
        let t = &a["tool_trace"][0];
        assert_eq!(t["tool"], "fs_read");
        assert_eq!(t["error"], "[omitted]");
    }

    #[tokio::test]
    async fn sensitive_keys_masked_in_args_and_result() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply(
            r#"{"tool": "notify_send", "params": {"title": "t", "api_key": "KEY-123", "nested": {"Authorization": "Bearer ZZZ"}}}"#,
        )
        .await;
        shim.push_ai_reply("done").await;
        shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        let a = &wait_for_chats_calls(&shim, "chat_turn_append", 1).await[0];
        let t = &a["tool_trace"][0];
        let args = t["args"].as_str().unwrap();
        assert!(!args.contains("KEY-123") && !args.contains("ZZZ"), "{args}");
        assert!(args.contains(r#""title":"t""#), "non-sensitive keys stay: {args}");
        assert!(args.contains("[redacted]"));
    }

    #[tokio::test]
    async fn failed_tool_and_unknown_tool_are_traced_with_error() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply(r#"{"tool": "no_such_tool", "params": {}}"#).await;
        shim.push_ai_reply("gave up").await;
        shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        let a = &wait_for_chats_calls(&shim, "chat_turn_append", 1).await[0];
        assert_eq!(a["tool_trace"][0]["tool"], "no_such_tool");
        assert!(a["tool_trace"][0]["error"].as_str().unwrap().contains("not in the operator"));
    }

    #[tokio::test]
    async fn needs_confirmation_appends_nothing_until_resume() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply(r#"{"tool": "fs_read", "params": {"path": "/tmp/x"}}"#).await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "read it"})).await.unwrap();
        assert_eq!(res["status"], "needs_confirmation");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(shim.chats_calls("chat_turn_append").await.is_empty(), "nothing while pending");
        shim.call("goal_resume", serde_json::json!({"id": res["id"], "approve": false}))
            .await
            .unwrap();
        let appended = wait_for_chats_calls(&shim, "chat_turn_append", 1).await;
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0]["status"], "cancelled");
    }

    #[tokio::test]
    async fn approved_resume_appends_the_final_turn_once() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply(r#"{"tool": "fs_read", "params": {"path": "/tmp/x"}}"#).await;
        shim.push_ai_reply("it says hi").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "read it"})).await.unwrap();
        shim.call("goal_resume", serde_json::json!({"id": res["id"], "approve": true}))
            .await
            .unwrap();
        let appended = wait_for_chats_calls(&shim, "chat_turn_append", 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(shim.chats_calls("chat_turn_append").await.len(), 1);
        assert_eq!(appended[0]["status"], "ok");
        assert_eq!(appended[0]["answer_text"], "it says hi");
        assert_eq!(appended[0]["tool_trace"][0]["tool"], "fs_read");
    }

    #[tokio::test]
    async fn llm_failure_appends_failed_turn() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        // No scripted replies: every chat_completion fails, even the text retry.
        let res = shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        assert_eq!(res["status"], "error");
        let a = &wait_for_chats_calls(&shim, "chat_turn_append", 1).await[0];
        assert_eq!(a["status"], "failed");
        assert_eq!(a["answer_text"], res["error"], "answer_text carries the error text");
        assert!(a["answer_text"].as_str().unwrap().contains("chat_completion"), "{a}");
    }

    #[tokio::test]
    async fn engine_error_ends_goal_as_error_and_still_records_the_turn() {
        let shim = start_plugin(Config::default()).await;
        shim.chats_normal("S1", true, vec![]).await;
        // 1st goal write (running) succeeds, the 2nd (after the answer) fails.
        shim.db.lock().await.fail_goal_set_at = Some(2);
        shim.push_ai_reply("an answer that cannot be saved").await;
        let err = shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap_err();
        assert!(err.contains("injected db_set failure"), "{err}");
        let a = &wait_for_chats_calls(&shim, "chat_turn_append", 1).await[0];
        assert_eq!(a["status"], "failed");
        // The model had already answered; that text is what the turn keeps.
        assert_eq!(a["answer_text"], "an answer that cannot be saved");
        // The recovery write (3rd) succeeded: the goal is not left "running".
        let got = shim.call("goal_get", serde_json::json!({"id": a["goal_id"]})).await.unwrap();
        assert_eq!(got["goal"]["status"], "error");
        assert!(got["goal"]["error"].as_str().unwrap().contains("injected db_set failure"));
    }

    #[tokio::test]
    async fn goal_without_session_appends_nothing() {
        let shim = start_plugin(Config::default()).await; // chats missing
        shim.push_ai_reply("ok").await;
        shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(shim.chats_calls("chat_turn_append").await.is_empty());
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
        // Dynamic catalog: no file words in the goal, so fs_* is filtered out.
        assert_eq!(names, vec!["notify_send", "secret_get", "ping_a", "ping_b"]);
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[tokio::test]
    async fn native_seed_does_not_duplicate_tool_catalog_as_text() {
        // Default NativeMode::Auto + non-empty catalog => native from the
        // first request. The seed prompt must not also carry the JSON
        // catalog `catalog_tools_param` already put in the `tools` param —
        // that's the duplication this change removes.
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply("All done.").await;

        let res = shim.call("goal_start", serde_json::json!({"goal": "notify me"})).await.unwrap();
        assert_eq!(res["status"], "completed", "{res}");

        let requests = shim.ai_requests().await;
        assert!(requests[0]["tools"].as_array().is_some(), "native tools param missing");
        let seed = requests[0]["messages"][0]["content"].as_str().unwrap();
        assert!(
            !seed.contains("requires_confirmation"),
            "native seed still embeds the JSON tool catalog: {seed}"
        );
        // Still carries enough for the model to know what's callable and
        // how to reply if it ever falls back to text.
        assert!(seed.contains("notify_send"), "tool groups overview must still name the tool: {seed}");
        assert!(seed.contains("EXACTLY ONE JSON object"), "reply protocol must remain: {seed}");
    }

    #[tokio::test]
    async fn text_mode_seed_unchanged_when_native_tools_off() {
        let shim = start_plugin(Config::default()).await;
        std::env::set_var("AGENT_PLUGIN_NATIVE_TOOLS", "off");
        shim.push_ai_reply("All done.").await;

        let res = shim.call("goal_start", serde_json::json!({"goal": "notify me"})).await.unwrap();
        std::env::remove_var("AGENT_PLUGIN_NATIVE_TOOLS");
        assert_eq!(res["status"], "completed", "{res}");

        let requests = shim.ai_requests().await;
        assert!(
            requests[0].get("tools").map(|t| t.as_array().unwrap().is_empty()).unwrap_or(true),
            "text mode must not send a native tools param"
        );
        let seed = requests[0]["messages"][0]["content"].as_str().unwrap();
        assert!(seed.contains("requires_confirmation"), "text-mode seed must carry the full JSON catalog: {seed}");
        assert!(seed.contains("notify_send"));
    }

    #[tokio::test]
    async fn native_rejection_injects_text_catalog_before_degraded_retry() {
        // THE TRAP: the seed went out native-only (no text catalog). If the
        // provider then rejects the `tools` param, the degraded text-only
        // retry must not be left with neither channel informed — the
        // engine must inject the text catalog into the transcript first.
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_failure("model does not support tools").await;
        shim.push_ai_reply("All done.").await;

        let res = shim.call("goal_start", serde_json::json!({"goal": "notify me"})).await.unwrap();
        assert_eq!(res["status"], "completed", "{res}");

        let requests = shim.ai_requests().await;
        assert_eq!(requests.len(), 2, "expected native attempt + degraded retry");
        assert!(requests[0]["tools"].as_array().is_some(), "first attempt must be native");
        assert!(
            requests[1].get("tools").map(|t| t.as_array().unwrap().is_empty()).unwrap_or(true),
            "retry after degradation must not resend tools natively"
        );
        let retry_msgs = requests[1]["messages"].as_array().unwrap();
        assert!(
            retry_msgs.iter().any(|m| m["content"].as_str().unwrap_or_default().contains("requires_confirmation")),
            "degraded retry transcript must carry the injected text tool catalog: {retry_msgs:?}"
        );
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

        // goal_list is a light projection: id/status/goal/timestamps/
        // step_count/final_answer/error, never the transcript and never
        // the full steps array. goal_get still returns everything.
        for g in list["goals"].as_array().unwrap() {
            assert!(g.get("transcript").is_none(), "{g}");
            assert!(g.get("steps").is_none(), "{g}");
            assert!(g.get("step_count").is_some(), "{g}");
            assert!(g.get("status").is_some(), "{g}");
            assert!(g.get("created_at_ms").is_some(), "{g}");
            assert!(g.get("updated_at_ms").is_some(), "{g}");
        }

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
        assert_eq!(names, vec!["notify_send", "fs_read", "secret_get", "ping_a", "ping_b"]);

        // notify_send has an operator entry, so its description and schema
        // stay exactly as written — but the entry left `risk` empty, and
        // that gap is filled from the plugin's manifest. Per-field merge,
        // not one layer winning wholesale.
        let notify = res["tools"].as_array().unwrap().iter()
            .find(|t| t["name"] == "notify_send").unwrap();
        assert_eq!(notify["source"], "merged");
        assert_eq!(notify["description"], "file curated notification");
        assert_eq!(notify["parameters"], serde_json::json!({"type": "object"}));
        assert_eq!(notify["risk"], "low");

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

    async fn handle_vec_upsert(store: &VecStore, params: Value) -> Result<Value, String> {
        let collection =
            params.get("collection").and_then(Value::as_str).unwrap_or_default().to_string();
        let id = params.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let text = params.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
        let metadata = params.get("metadata").cloned().unwrap_or(Value::Null);
        store
            .lock()
            .await
            .entry(collection)
            .or_default()
            .push(serde_json::json!({"id": id, "text": text, "metadata": metadata}));
        Ok(serde_json::json!({"ok": true}))
    }

    async fn handle_vec_delete(store: &VecStore, params: Value) -> Result<Value, String> {
        let collection =
            params.get("collection").and_then(Value::as_str).unwrap_or_default().to_string();
        let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
        let mut store = store.lock().await;
        // Test hook: ids listed in the pseudo-collection `__fail_delete__` fail.
        let doomed = store
            .get("__fail_delete__")
            .is_some_and(|v| v.iter().any(|d| d["id"].as_str() == Some(id)));
        if doomed {
            return Err(format!("vec_delete injected failure for {id}"));
        }
        let docs = store.entry(collection).or_default();
        let before = docs.len();
        docs.retain(|d| d["id"].as_str() != Some(id));
        Ok(serde_json::json!({"deleted": docs.len() != before}))
    }

    /// Test hooks live in pseudo-collections: `__embed__` (a doc whose id is
    /// the model name; absent = `vec_embed` unavailable), `__fail_vector__`
    /// (docs whose id is a collection whose vector queries fail),
    /// `__vec_log__` (journal of `vec_embed`/`vec_query` calls).
    async fn vec_log(store: &VecStore, action: &str, params: &Value) {
        store
            .lock()
            .await
            .entry("__vec_log__".to_string())
            .or_default()
            .push(serde_json::json!({"action": action, "params": params}));
    }

    async fn handle_vec_embed(store: &VecStore, params: Value) -> Result<Value, String> {
        vec_log(store, "vec_embed", &params).await;
        let model = store
            .lock()
            .await
            .get("__embed__")
            .and_then(|d| d.first())
            .and_then(|d| d["id"].as_str().map(str::to_string));
        match model {
            None => Err("action not found".to_string()),
            Some(m) => {
                let n = params["texts"].as_array().map_or(1, Vec::len);
                Ok(serde_json::json!({
                    "vectors": vec![vec![1.0, 0.0, 0.5, 0.0]; n], "model": m, "dim": 4
                }))
            }
        }
    }

    async fn handle_vec_query(store: &VecStore, params: Value) -> Result<Value, String> {
        vec_log(store, "vec_query", &params).await;
        let collection =
            params.get("collection").and_then(Value::as_str).unwrap_or_default().to_string();
        if params.get("vector").is_some()
            && store
                .lock()
                .await
                .get("__fail_vector__")
                .is_some_and(|d| d.iter().any(|x| x["id"].as_str() == Some(&collection)))
        {
            return Err("dimension mismatch".to_string());
        }
        let facts = {
            let store = store.lock().await;
            store.get(&collection).cloned().unwrap_or_default()
        };
        let results: Vec<Value> = facts
            .iter()
            .map(|f| {
                serde_json::json!({
                    "id": f["id"].clone(),
                    "score": 1.0,
                    "text": f["text"].clone(),
                    "metadata": f["metadata"].clone(),
                })
            })
            .collect();
        Ok(serde_json::json!({"results": results}))
    }

    /// Scoped `AGENT_PLUGIN_MEMORY` override: tests share one process env, so
    /// a leaked `=on` would flip recall/remember on for every concurrently
    /// running goal test. Restores the prior values on drop (including panic).
    struct MemoryEnvGuard {
        prev_memory: Option<String>,
        prev_collection: Option<String>,
    }

    impl MemoryEnvGuard {
        fn enable(collection: &str) -> Self {
            let guard = Self {
                prev_memory: std::env::var("AGENT_PLUGIN_MEMORY").ok(),
                prev_collection: std::env::var("AGENT_PLUGIN_MEMORY_COLLECTION").ok(),
            };
            std::env::set_var("AGENT_PLUGIN_MEMORY", "on");
            std::env::set_var("AGENT_PLUGIN_MEMORY_COLLECTION", collection);
            guard
        }
    }

    impl Drop for MemoryEnvGuard {
        fn drop(&mut self) {
            match &self.prev_memory {
                Some(v) => std::env::set_var("AGENT_PLUGIN_MEMORY", v),
                None => std::env::remove_var("AGENT_PLUGIN_MEMORY"),
            }
            match &self.prev_collection {
                Some(v) => std::env::set_var("AGENT_PLUGIN_MEMORY_COLLECTION", v),
                None => std::env::remove_var("AGENT_PLUGIN_MEMORY_COLLECTION"),
            }
        }
    }

    async fn wait_for_remembered_fact(shim: &Shim, collection: &str, fact: &str) -> bool {
        for _ in 0..120 {
            let facts = shim.vec_facts(collection).await;
            if facts.iter().any(|f| {
                f["metadata"].get("fact").and_then(Value::as_str) == Some(fact)
                    && f["metadata"].get("goal_id").and_then(Value::as_str).is_some()
            }) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn memory_recalls_context_and_remembers_facts() {
        let shim = start_plugin(Config::default()).await;
        let _mem = MemoryEnvGuard::enable("agent-memory-test");

        shim.seed_fact("agent-memory-test", "fseed-0", "the user runs Arch Linux").await;
        shim.seed_index("fseed-0").await;

        // Script the goal loop's final answer, then the extraction pass's
        // facts array (both ride chat_completion).
        shim.push_ai_reply("The user runs Arch Linux.").await;
        shim.push_ai_reply("[\"the user runs Arch Linux\"]").await;

        let res = shim
            .call("goal_start", serde_json::json!({"goal": "which OS does the user run"}))
            .await
            .unwrap();
        assert_eq!(res["status"], "completed", "{res}");

        // (recall) the seeded fact reached the LLM as a leading context turn.
        let reqs = shim.ai_requests().await;
        assert!(!reqs.is_empty(), "goal chat_completion never fired");
        let messages = reqs[0]["messages"].as_array().expect("chat had no messages");
        let context_turn = messages
            .iter()
            .find(|m| m["content"].as_str().unwrap_or_default().contains("[KNOWN CONTEXT]"))
            .expect("memory recall block missing from the LLM call");
        assert!(context_turn["content"].as_str().unwrap().contains("Arch Linux"));

        // (remember) the detached extraction stored the fact via vec_upsert —
        // distinguished from the seed by its goal_id metadata.
        let remembered =
            wait_for_remembered_fact(&shim, "agent-memory-test", "the user runs Arch Linux").await;
        assert!(remembered, "vec_upsert should have stored the remembered fact");
    }

    async fn seed_session_fact(shim: &Shim, id: &str, session: Option<&str>) {
        shim.seed_fact("agent-memory-test", id, &format!("fact {id}")).await;
        let mut db = shim.db.lock().await;
        let key = agent_plugin::memory::INDEX_KEY.to_string();
        let mut entries: Vec<Value> =
            db.kv.get(&key).and_then(Value::as_array).cloned().unwrap_or_default();
        let mut e = serde_json::json!({"id": id, "ts": 0});
        if let Some(sid) = session {
            e["session_id"] = serde_json::json!(sid);
        }
        entries.push(e);
        db.kv.insert(key, Value::Array(entries));
    }

    #[tokio::test]
    async fn remember_tags_facts_with_session() {
        let shim = start_plugin(Config::default()).await;
        let _mem = MemoryEnvGuard::enable("agent-memory-test");
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply("The user runs Arch Linux.").await;
        shim.push_ai_reply("[\"the user runs Arch Linux\"]").await;
        shim.call("goal_start", serde_json::json!({"goal": "which OS"})).await.unwrap();
        assert!(
            wait_for_remembered_fact(&shim, "agent-memory-test", "the user runs Arch Linux").await
        );
        let facts = shim.vec_facts("agent-memory-test").await;
        let f = facts.iter().find(|f| f["metadata"]["goal_id"].is_string()).unwrap();
        assert_eq!(f["metadata"]["session_id"], "S1");
        let idx = shim.call("memory_list", serde_json::json!({})).await.unwrap();
        assert_eq!(idx["facts"][0]["session_id"], "S1");
    }

    #[tokio::test]
    async fn memory_forget_session_deletes_only_that_session() {
        let shim = start_plugin(Config::default()).await;
        let _mem = MemoryEnvGuard::enable("agent-memory-test");
        seed_session_fact(&shim, "a1", Some("S1")).await;
        seed_session_fact(&shim, "a2", Some("S1")).await;
        seed_session_fact(&shim, "b1", Some("S2")).await;
        seed_session_fact(&shim, "old", None).await; // pre-sessions entry: no session_id
        let res = shim
            .call("memory_forget_session", serde_json::json!({"session_id": "S1"}))
            .await
            .unwrap();
        assert_eq!(res["deleted"], 2);
        let left: Vec<String> = shim
            .vec_facts("agent-memory-test")
            .await
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(left, ["b1", "old"]);
        let idx = shim.call("memory_list", serde_json::json!({})).await.unwrap();
        let ids: Vec<&str> =
            idx["facts"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"b1") && ids.contains(&"old"));
        // Repeat and unknown session: nothing to do, not an error.
        let again = shim
            .call("memory_forget_session", serde_json::json!({"session_id": "S1"}))
            .await
            .unwrap();
        assert_eq!(again["deleted"], 0);
        let unknown = shim
            .call("memory_forget_session", serde_json::json!({"session_id": "NOPE"}))
            .await
            .unwrap();
        assert_eq!(unknown["deleted"], 0);
    }

    #[tokio::test]
    async fn forget_session_partial_failure_still_rewrites_the_index() {
        let shim = start_plugin(Config::default()).await;
        let _mem = MemoryEnvGuard::enable("agent-memory-test");
        seed_session_fact(&shim, "a1", Some("S1")).await;
        seed_session_fact(&shim, "a2", Some("S1")).await;
        seed_session_fact(&shim, "a3", Some("S1")).await;
        seed_session_fact(&shim, "b1", Some("S2")).await;
        shim.seed_fact("__fail_delete__", "a2", "boom").await;
        let err = shim
            .call("memory_forget_session", serde_json::json!({"session_id": "S1"}))
            .await
            .unwrap_err();
        assert!(err.contains("injected failure"), "{err}");
        let idx = shim.call("memory_list", serde_json::json!({})).await.unwrap();
        let mut ids: Vec<&str> =
            idx["facts"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
        ids.sort();
        // a1 was deleted before the failure: gone from the index; a2 (failed), a3 (not tried) remain.
        assert_eq!(ids, ["a2", "a3", "b1"]);
        let left: Vec<String> = shim.vec_facts("agent-memory-test").await.iter()
            .map(|f| f["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(left, ["a2", "a3", "b1"]);
        // Once the fault is gone a retry finishes the job.
        shim.vec_store.lock().await.remove("__fail_delete__");
        let ok = shim
            .call("memory_forget_session", serde_json::json!({"session_id": "S1"}))
            .await
            .unwrap();
        assert_eq!(ok["deleted"], 2);
    }

    #[tokio::test]
    async fn goal_delete_removes_doc_and_is_idempotent() {
        let shim = start_plugin(Config::default()).await;
        shim.push_ai_reply("ok").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "g"})).await.unwrap();
        let id = res["id"].clone();
        let del = shim.call("goal_delete", serde_json::json!({"id": id})).await.unwrap();
        assert_eq!(del["deleted"], true);
        let got = shim.call("goal_get", serde_json::json!({"id": id})).await.unwrap();
        assert_eq!(got["found"], false);
        let again = shim.call("goal_delete", serde_json::json!({"id": id})).await.unwrap();
        assert_eq!(again["deleted"], false);
    }
    /// Scoped `AGENT_PLUGIN_EMBEDDING_FILTER=on` (restored on drop).
    struct EmbedFilterGuard(Option<String>);

    impl EmbedFilterGuard {
        fn enable() -> Self {
            let g = Self(std::env::var("AGENT_PLUGIN_EMBEDDING_FILTER").ok());
            std::env::set_var("AGENT_PLUGIN_EMBEDDING_FILTER", "on");
            g
        }
    }

    impl Drop for EmbedFilterGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("AGENT_PLUGIN_EMBEDDING_FILTER", v),
                None => std::env::remove_var("AGENT_PLUGIN_EMBEDDING_FILTER"),
            }
        }
    }

    async fn seed_tool_hits(shim: &Shim) {
        for t in ["notify_send", "fs_read", "secret_get"] {
            shim.seed_fact("agent-tools", t, t).await;
        }
    }

    fn text_of(v: &Value) -> bool {
        v.get("text").is_some() && v.get("vector").is_none()
    }

    fn vector_of(v: &Value) -> bool {
        v.get("vector").is_some() && v.get("text").is_none()
    }

    #[tokio::test]
    async fn one_embedding_per_goal() {
        let shim = start_plugin(Config::default()).await;
        let _mem = MemoryEnvGuard::enable("agent-memory-test");
        let _emb = EmbedFilterGuard::enable();
        shim.embed_model("m1").await;
        seed_tool_hits(&shim).await;
        shim.seed_fact("agent-memory-test", "f1", "the user runs Arch Linux").await;
        shim.seed_index("f1").await;
        shim.push_ai_reply("ok").await;
        shim.push_ai_reply("[]").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "which OS"})).await.unwrap();
        assert_eq!(res["status"], "completed", "{res}");
        let embeds = shim.vec_calls("vec_embed").await;
        assert_eq!(embeds.len(), 1, "{embeds:?}");
        assert_eq!(embeds[0], serde_json::json!({"texts": ["which OS"]}));
        let queries = shim.vec_calls("vec_query").await;
        let on = |c: &str| -> Vec<Value> {
            queries.iter().filter(|q| q["collection"] == c).cloned().collect()
        };
        assert_eq!(on("agent-tools").len(), 1, "{queries:?}");
        assert_eq!(on("agent-memory-test").len(), 1, "{queries:?}");
        assert!(queries.iter().all(vector_of), "text query sneaked in: {queries:?}");
    }

    #[tokio::test]
    async fn fake_embedding_falls_back_to_text_queries() {
        let cfg = Config::default();
        let shim = start_plugin(cfg).await;
        let _mem = MemoryEnvGuard::enable("agent-memory-test");
        let _emb = EmbedFilterGuard::enable();
        shim.embed_model("fake").await;
        seed_tool_hits(&shim).await;
        shim.seed_fact("agent-memory-test", "f1", "the user runs Arch Linux").await;
        shim.seed_index("f1").await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply("ok").await;
        shim.push_ai_reply("[]").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "which OS"})).await.unwrap();
        assert_eq!(res["status"], "completed", "{res}");
        let queries = shim.vec_calls("vec_query").await;
        assert_eq!(queries.len(), 2, "{queries:?}");
        assert!(queries.iter().all(text_of), "{queries:?}");
        assert!(shim.chats_calls("chat_recall").await.is_empty(), "no vector, no recall");
        let msgs = shim.ai_requests().await[0]["messages"].clone();
        assert!(msgs.to_string().contains("Arch Linux"), "facts still reach the model");
    }

    #[tokio::test]
    async fn vector_query_error_retries_with_text() {
        let shim = start_plugin(Config::default()).await;
        let _emb = EmbedFilterGuard::enable();
        shim.embed_model("m1").await;
        seed_tool_hits(&shim).await;
        shim.seed_fact("__fail_vector__", "agent-tools", "").await;
        shim.push_ai_reply("ok").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "hello"})).await.unwrap();
        assert_eq!(res["status"], "completed", "{res}");
        let q: Vec<Value> = shim
            .vec_calls("vec_query")
            .await
            .into_iter()
            .filter(|q| q["collection"] == "agent-tools")
            .collect();
        assert_eq!(q.len(), 2, "{q:?}");
        assert!(vector_of(&q[0]) && text_of(&q[1]), "{q:?}");
    }

    fn past_hit(id: &str, title: &str) -> Value {
        serde_json::json!({"session_id": id, "title": title, "summary": "sunny in Tashkent",
            "at": 1_793_880_000_000_i64, "score": 0.9})
    }

    fn goal_message(req: &Value) -> String {
        req["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn recall_block_goes_into_goal_message() {
        // Baseline: recall off.
        let off = start_plugin(Config { recall: false, ..Config::default() }).await;
        off.embed_model("m1").await;
        off.chats_normal("S1", false, vec![turn(1, "q1", "a1", "ok")]).await;
        off.chats_recall_hits(vec![past_hit("P1", "Weather")]).await;
        off.push_ai_reply("ok").await;
        off.call("goal_start", serde_json::json!({"goal": "weather?"})).await.unwrap();
        let base = off.ai_requests().await[0]["messages"].clone();
        assert!(off.chats_calls("chat_recall").await.is_empty());
        drop(off);

        let shim = start_plugin(Config::default()).await;
        shim.embed_model("m1").await;
        shim.chats_normal("S1", false, vec![turn(1, "q1", "a1", "ok")]).await;
        shim.chats_recall_hits(vec![past_hit("P1", "Weather")]).await;
        shim.push_ai_reply("ok").await;
        let res = shim.call("goal_start", serde_json::json!({"goal": "weather?"})).await.unwrap();
        let req = shim.ai_requests().await[0].clone();
        let msgs = req["messages"].as_array().unwrap();
        let base = base.as_array().unwrap();
        assert_eq!(msgs.len(), base.len(), "no extra message");
        assert_eq!(msgs[0], base[0], "role message untouched");
        for w in msgs.windows(2) {
            assert_ne!(w[0]["role"], w[1]["role"], "roles alternate");
        }
        let goal = goal_message(&req);
        assert!(goal.starts_with("weather?\n\n---\n[PAST SESSIONS]"), "{goal}");
        assert!(goal.contains("- 2026-11-05 — Weather: sunny in Tashkent"), "{goal}");
        let got = shim.call("goal_get", serde_json::json!({"id": res["id"]})).await.unwrap();
        assert_eq!(got["goal"]["recalled_sessions"], serde_json::json!(["P1"]));
    }

    #[tokio::test]
    async fn recall_excludes_current_session_and_passes_cfg() {
        let shim = start_plugin(Config::default()).await;
        shim.embed_model("m1").await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.push_ai_reply("ok").await;
        shim.call("goal_start", serde_json::json!({"goal": "weather?"})).await.unwrap();
        let calls = shim.chats_calls("chat_recall").await;
        assert_eq!(calls.len(), 1, "{calls:?}");
        let c = &calls[0];
        assert_eq!(c["exclude_session_id"], "S1");
        assert_eq!(c["top_k"], 3);
        assert_eq!(c["min_score"], 0.72);
        assert_eq!(c["model"], "m1");
        assert_eq!(c["vector"].as_array().unwrap().len(), 4);
        let mut keys: Vec<&String> = c.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["exclude_session_id", "min_score", "model", "top_k", "vector"]);
    }

    #[tokio::test]
    async fn slow_recall_is_dropped_within_budget() {
        let cfg = Config { recall_budget_ms: 150, ..Config::default() };
        let shim = start_plugin(cfg).await;
        shim.embed_model("m1").await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.chats.lock().await.recall_silent = true;
        shim.push_ai_reply("ok").await;
        let t = std::time::Instant::now();
        let res = shim.call("goal_start", serde_json::json!({"goal": "weather?"})).await.unwrap();
        let took = t.elapsed();
        assert_eq!(res["status"], "completed", "{res}");
        assert_eq!(shim.chats_calls("chat_recall").await.len(), 1);
        assert_eq!(goal_message(&shim.ai_requests().await[0]), "weather?");
        assert!(took < Duration::from_millis(150 + 50 + 400), "turn took {took:?}");
    }

    #[tokio::test]
    async fn missing_chats_recall_gives_no_block() {
        // chats absent entirely: every chat_* call answers `action not found`.
        let shim = start_plugin(Config::default()).await;
        shim.embed_model("m1").await;
        shim.push_ai_reply("ok").await;
        shim.push_ai_reply("ok").await;
        for _ in 0..2 {
            let res = shim.call("goal_start", serde_json::json!({"goal": "hi"})).await.unwrap();
            assert_eq!(res["status"], "completed", "{res}");
        }
        for r in shim.ai_requests().await {
            assert_eq!(goal_message(&r), "hi");
        }
    }

    #[tokio::test]
    async fn recall_off_makes_no_recall_call() {
        let shim = start_plugin(Config { recall: false, ..Config::default() }).await;
        shim.embed_model("m1").await;
        shim.chats_normal("S1", true, vec![]).await;
        shim.chats_recall_hits(vec![past_hit("P1", "Weather")]).await;
        shim.push_ai_reply("ok").await;
        shim.call("goal_start", serde_json::json!({"goal": "weather?"})).await.unwrap();
        assert!(shim.chats_calls("chat_recall").await.is_empty());
        assert_eq!(goal_message(&shim.ai_requests().await[0]), "weather?");
    }
}

#[cfg(test)]
mod manifest_specs_tests {
    use super::ACTIONS;
    use serde_json::Value;

    // Regression guard: the agent builds the model's tool catalog from
    // every plugin's action_specs — including its own. An undocumented or
    // unrisked action here is the catalog gap the agent is supposed to
    // detect, shipped by the detector itself.
    //
    // Reads the shipped plugin.json directly (not action_specs(), which
    // resolves via current_exe() and returns an empty list in a dev-build
    // test binary — it cannot tell correct wiring from none at all).
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

        // ActionRisk::Unknown is what an absent or unparseable `risk` key
        // degrades to; an action that lands there falls back to the
        // agent's name-shaped inference instead of its owner's judgement.
        let unrisked: Vec<&str> = specs
            .iter()
            .filter(|s| s.risk == vynkor_sdk::proto::ActionRisk::Unknown as i32)
            .map(|s| s.name.as_str())
            .collect();
        assert!(unrisked.is_empty(), "actions missing a risk label: {unrisked:?}");
    }

    // The manifest's action list and the dispatcher's must not drift: a
    // name in one and not the other is either an undocumented live action
    // or a documented tool that errors with "unknown action" when called.
    #[test]
    fn shipped_manifest_lists_exactly_the_dispatched_actions() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let mut documented: Vec<String> = parsed["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap().to_string())
            .collect();
        documented.sort();
        let mut dispatched: Vec<String> = ACTIONS.iter().map(|s| s.to_string()).collect();
        dispatched.sort();
        assert_eq!(documented, dispatched);
    }

}
