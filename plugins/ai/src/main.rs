//! `ai` plugin — provider-agnostic chat completion for other plugins, routed
//! through `network`'s `http_request` action rather than opening its own
//! sockets (see ROADMAP.md, "Decision: reuse `network`, don't reinvent").
//!
//! v0.3 adds a SQLite store (`VYN_DATA_DIR/ai.db`) holding declared +
//! auto-discovered models, agent profiles, and per-call token usage.
//!
//! Doesn't use the SDK's `Plugin::run`/`serve` loop or its
//! `concurrent::serve_concurrent` (used by `database`/`network`): neither
//! gives a handler task a second connection for the outbound `send_action`
//! call into `network` — the kernel rejects a second registration under the
//! same `plugin_id` (`vynkor/src/plugins/registry.rs`), and
//! `concurrent::ConcurrentHandler::on_action` deliberately never touches the
//! client at all (see that module's doc comment). `ai`'s handlers need
//! exactly that: an outbound call per request.
//!
//! So this plugin drives its own concurrent loop (`outbound.rs` has the
//! full rationale): one task owns the single `VynkorClient` exclusively,
//! `tokio::select!`ing between inbound frames, completed inbound-request
//! replies, and outbound-call requests from spawned handler tasks. Each
//! inbound `ActionRequest` (`chat_completion`, `embedding`, ...) is spawned
//! onto its own task immediately, so N goals in flight run their provider
//! HTTP round-trips concurrently instead of queuing behind each other — and
//! critically, the loop keeps answering kernel `Ping`s the whole time, so a
//! slow provider call no longer trips the supervisor's watchdog and gets
//! the whole plugin SIGKILLed (see `docs/`/incident notes: this used to
//! happen every 5-30 minutes under normal use).
//!
//! Before this, the plugin was sequential, one request at a time — same
//! model `network` and `ping-pong-rs` used before their own migration to
//! `concurrent::serve_concurrent`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ai_plugin::outbound::{ActionCaller, OutboundCall, OutboundHandle};
use ai_plugin::{config, db, discovery, handler};
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

/// How often the loop sweeps `pending` for outbound calls that timed out
/// without a matching `ActionResponse` ever arriving (e.g. `network` itself
/// wedged). Coarse on purpose — timeouts here are seconds, not
/// milliseconds, so 500ms of slack is invisible to callers.
const PENDING_SWEEP_INTERVAL: Duration = Duration::from_millis(500);
/// Mirrors `VynkorClient::send_action`'s own default (`timeout_ms == 0`).
const DEFAULT_OUTBOUND_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on in-flight inbound requests / outbound calls / queued replies.
/// Generous: a stalled provider should back up here, loudly, rather than
/// silently unbounded-queue.
const CHANNEL_CAPACITY: usize = 256;

const PLUGIN_ID: &str = "ai";
const PLUGIN_VERSION: &str = "0.1.2";

/// Startup model-refresh retries: the first attempts typically race the
/// `network` plugin's registration (`ActionNotFound`).
const STARTUP_REFRESH_ATTEMPTS: u32 = 5;

fn manifest() -> PluginManifest {
    PluginManifest {
        // `network`: ai invokes `network`'s gated `http_request` action, and
        // `secrets`: ai resolves provider keys from the secrets vault first
        // (`secret_get`, gated by PERMISSION_SECRETS). T-19 requires callers
        // of a gated action to hold its permission too (matches plugin.json
        // `permissions`; Manifest v2 per-action model).
        permissions: vec!["PERMISSION_NETWORK".into(), "PERMISSION_SECRETS".into()],
        actions: vec![
            "chat_completion".to_string(),
            "embedding".to_string(),
            "list_models".to_string(),
            "list_agents".to_string(),
            "refresh_models".to_string(),
            "usage_stats".to_string(),
        ],
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

async fn handle_action_request(
    caller: &mut impl ActionCaller,
    req: ActionRequest,
    db: &db::AiDb,
    cfg: &config::AiConfig,
) -> Envelope {
    let outcome = match req.action.as_str() {
        "chat_completion" => handler::handle_chat_completion(caller, &req.params_json, db).await,
        "embedding" => handler::handle_embedding(caller, &req.params_json, db).await,
        "list_models" => handler::handle_list_models(db),
        "list_agents" => handler::handle_list_agents(db),
        "usage_stats" => handler::handle_usage_stats(db),
        "refresh_models" => handler::handle_refresh_models(caller, db, &cfg.discovery).await,
        other => {
            return Envelope {
                payload: Some(envelope::Payload::ActionResponse(ActionResponse {
                    action_id: req.action_id,
                    status: ActionStatus::ActionNotFound as i32,
                    data_json: Vec::new(),
                    error: format!("unknown action: {other}"),
                })),
                ..Default::default()
            };
        }
    };
    let reply = match outcome {
        Ok(data_json) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionOk as i32,
            data_json,
            error: String::new(),
        },
        Err(error) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionError as i32,
            data_json: Vec::new(),
            error,
        },
    };
    Envelope {
        payload: Some(envelope::Payload::ActionResponse(reply)),
        ..Default::default()
    }
}

async fn serve(
    mut client: VynkorClient,
    db: Arc<db::AiDb>,
    cfg: config::AiConfig,
) -> Result<(), VynkorError> {
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

    if let Err(e) = config::seed(&db, &cfg) {
        eprintln!("[{PLUGIN_ID}] failed to seed config: {e}");
    }

    if !cfg.discovery.is_empty() {
        for _ in 0..STARTUP_REFRESH_ATTEMPTS {
            match handler::handle_refresh_models(&mut client, &db, &cfg.discovery).await {
                Ok(data) => match serde_json::from_slice::<discovery::Discovered>(&data) {
                    Ok(d) => {
                        println!(
                            "[{PLUGIN_ID}] models refreshed: {} new, {} updated",
                            d.discovered, d.updated
                        );
                        for e in &d.errors {
                            eprintln!("[{PLUGIN_ID}] discovery error: {e}");
                        }
                        if d.errors.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("[{PLUGIN_ID}] failed to decode refresh result: {e}");
                        break;
                    }
                },
                Err(e) => eprintln!("[{PLUGIN_ID}] initial model refresh failed: {e}"),
            }
            // The startup refresh races the `network` plugin's registration
            // (ActionNotFound) — back off and retry rather than dying on it.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    // Tie the default model to the default agent's model when no model
    // default is configured explicitly (fresh installs land on the
    // operator's chosen default agent instead of the first alphabetical id).
    if db.default_model().map(|m| m.is_none()).unwrap_or(true) {
        if let Ok(Some(agent)) = db.default_agent() {
            if db
                .get_model(&agent.model_id)
                .map(|m| m.is_some())
                .unwrap_or(false)
            {
                let _ = db.set_model_default(&agent.model_id);
            }
        }
    }

    run_loop(client, db, Arc::new(cfg)).await
}

/// The concurrent message loop itself, split out from [`serve`] so tests can
/// drive it directly against a pre-registered client (mirrors
/// `concurrent::run_concurrent_loop`'s own test seam) without the
/// registration handshake and startup model refresh.
async fn run_loop(
    mut client: VynkorClient,
    db: Arc<db::AiDb>,
    cfg: Arc<config::AiConfig>,
) -> Result<(), VynkorError> {
    // Replies to inbound ActionRequests (chat_completion, ...), completed by
    // spawned handler tasks — the loop below is the only thing that ever
    // touches `client`, so tasks hand their result back over this channel
    // instead of sending it themselves.
    let (resp_tx, mut resp_rx) = mpsc::channel::<Envelope>(CHANNEL_CAPACITY);
    // Outbound calls a handler task wants made on its behalf (see
    // `outbound.rs`) — `OutboundHandle` is the `ActionCaller` handler tasks
    // actually hold.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<OutboundCall>(CHANNEL_CAPACITY);
    // In-flight outbound calls this loop has sent to the kernel on a
    // handler's behalf, keyed by the `action_id` it minted, waiting for the
    // matching `ActionResponse` to route back through the oneshot.
    let mut pending: HashMap<String, (oneshot::Sender<Result<ActionResponse, VynkorError>>, Instant)> =
        HashMap::new();
    let mut sweep = tokio::time::interval(PENDING_SWEEP_INTERVAL);

    loop {
        tokio::select! {
            envelope = client.recv() => {
                let env = match envelope {
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
                        // Answered straight off the select! loop, never
                        // queued behind a handler task — this is the whole
                        // point: no chat_completion in flight can ever
                        // delay a Pong past the watchdog's deadline again.
                        let _ = client.send("kernel", pong).await;
                    }
                    Some(envelope::Payload::PluginShutdown(_)) => break,
                    Some(envelope::Payload::Event(event)) => {
                        // ai declares no event subscriptions; ack defensively
                        // so the kernel doesn't retry anything unexpectedly
                        // delivered.
                        let _ = client.ack_event(&event.event_id).await;
                    }
                    Some(envelope::Payload::ActionRequest(req)) => {
                        // Someone (agent, tts, ...) is calling *us* — spawn a
                        // task so it runs concurrently with everything else.
                        let db = db.clone();
                        let cfg = cfg.clone();
                        let resp_tx = resp_tx.clone();
                        let mut caller = OutboundHandle::new(outbound_tx.clone());
                        tokio::spawn(async move {
                            let resp = handle_action_request(&mut caller, req, &db, &cfg).await;
                            let _ = resp_tx.send(resp).await;
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        // The reply to a call *we* made on a handler task's
                        // behalf (network's http_request, secrets' secret_get,
                        // ...) — route it back to whichever task is waiting.
                        if let Some((reply, _)) = pending.remove(&resp.action_id) {
                            let _ = reply.send(Ok(resp));
                        } else {
                            eprintln!(
                                "[{PLUGIN_ID}] stray ActionResponse for unknown action_id {}",
                                resp.action_id
                            );
                        }
                    }
                    other => {
                        println!("[{PLUGIN_ID}] unhandled message: {other:?}");
                    }
                }
            }
            Some(resp) = resp_rx.recv() => {
                let _ = client.send("kernel", resp).await;
            }
            Some(call) = outbound_rx.recv() => {
                let action_id = uuid::Uuid::new_v4().to_string();
                let timeout = if call.timeout_ms == 0 {
                    DEFAULT_OUTBOUND_TIMEOUT
                } else {
                    Duration::from_millis(call.timeout_ms as u64)
                };
                let env = Envelope {
                    payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                        action_id: action_id.clone(),
                        action: call.action,
                        params_json: call.params_json,
                        timeout_ms: call.timeout_ms,
                        streaming: false,
                        ..Default::default()
                    })),
                    ..Default::default()
                };
                match client.send("kernel", env).await {
                    Ok(()) => {
                        pending.insert(action_id, (call.reply, Instant::now() + timeout));
                    }
                    Err(e) => {
                        let _ = call.reply.send(Err(e));
                    }
                }
            }
            _ = sweep.tick() => {
                let now = Instant::now();
                let expired: Vec<String> = pending
                    .iter()
                    .filter(|(_, (_, deadline))| *deadline <= now)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in expired {
                    if let Some((reply, _)) = pending.remove(&id) {
                        let _ = reply.send(Err(VynkorError::Timeout));
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
    let socket_path = std::env::var("VYN_SOCKET_PATH")
        .unwrap_or_else(|_| vynkor_wire::socket::default_socket_path());
    let secret = std::env::var("VYN_JWT_SECRET")
        .ok()
        .filter(|s| !s.is_empty());
    let client = match secret {
        Some(s) => VynkorClient::connect_with_secret(&socket_path, s.as_bytes()).await?,
        None => VynkorClient::connect(&socket_path).await?,
    };

    let data_dir = std::env::var_os("VYN_DATA_DIR").map(PathBuf::from);
    let db = match db::AiDb::open(data_dir.as_deref()) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            eprintln!("[{PLUGIN_ID}] cannot open database: {e}");
            std::process::exit(1);
        }
    };
    let cfg = config::from_env();

    serve(client, db, cfg).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use tokio::net::UnixStream;
    use vynkor_sdk::proto::Ping;

    fn chat_params(tag: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "provider": "openai",
            "base_url": "http://fake/v1",
            "model": "test-model",
            "api_key_env": "TEST_KEY",
            "messages": [{"role": "user", "content": tag}],
        }))
        .unwrap()
    }

    fn ok_completion_body() -> String {
        serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })
        .to_string()
    }

    async fn recv_action_request(kernel: &mut VynkorClient) -> ActionRequest {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for ActionRequest")
                .unwrap();
            if let Some(envelope::Payload::ActionRequest(req)) = env.payload {
                return req;
            }
        }
    }

    /// Collect `count` outbound `http_request` calls, transparently
    /// answering any `secret_get` calls (key_resolve's vault hop, which
    /// every `chat_completion` makes first) with "not found" so the caller
    /// falls back to its env var. Returns once `count` `http_request` calls
    /// have arrived, still unanswered.
    async fn collect_http_requests(kernel: &mut VynkorClient, count: usize) -> Vec<ActionRequest> {
        let mut out = Vec::new();
        while out.len() < count {
            let req = recv_action_request(kernel).await;
            match req.action.as_str() {
                "secret_get" => {
                    let resp = Envelope {
                        payload: Some(envelope::Payload::ActionResponse(ActionResponse {
                            action_id: req.action_id,
                            status: ActionStatus::ActionOk as i32,
                            data_json: serde_json::to_vec(&serde_json::json!({"found": false}))
                                .unwrap(),
                            error: String::new(),
                        })),
                        ..Default::default()
                    };
                    kernel.send("client", resp).await.unwrap();
                }
                "http_request" => out.push(req),
                other => panic!("unexpected outbound action: {other}"),
            }
        }
        out
    }

    async fn recv_action_response(kernel: &mut VynkorClient) -> ActionResponse {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for ActionResponse")
                .unwrap();
            if let Some(envelope::Payload::ActionResponse(resp)) = env.payload {
                return resp;
            }
        }
    }

    /// Round-trips a `Ping`, failing the test if `Pong` doesn't come back
    /// promptly — this is the exact liveness check the kernel's watchdog
    /// performs, and the exact one used to go unanswered (and get the whole
    /// plugin SIGKILLed) while a slow chat_completion was in flight.
    async fn assert_ping_answered_promptly(kernel: &mut VynkorClient) {
        let env = Envelope {
            payload: Some(envelope::Payload::Ping(Ping { timestamp: 42 })),
            ..Default::default()
        };
        kernel.send("client", env).await.unwrap();
        loop {
            let env = tokio::time::timeout(Duration::from_millis(500), kernel.recv())
                .await
                .expect("Pong did not arrive promptly — Ping got stuck behind a handler task")
                .unwrap();
            if let Some(envelope::Payload::Pong(pong)) = env.payload {
                assert_eq!(pong.original_timestamp, 42);
                return;
            }
        }
    }

    /// Regression test for the incident this module's doc comment
    /// describes: two `chat_completion` calls used to serialize behind one
    /// exclusive client, and a slow provider round-trip blocked `Ping`
    /// replies past the watchdog's deadline. Proves both properties of the
    /// fix: N inbound requests run concurrently (both outbound `http_request`
    /// calls are in flight before either is answered), and `Ping` is
    /// answered immediately regardless.
    #[tokio::test]
    async fn concurrent_chat_completions_dont_block_each_other_or_ping() {
        std::env::set_var(ai_plugin::request::ALLOWED_KEY_ENVS_ENV, "TEST_KEY");
        std::env::set_var("TEST_KEY", "test-key-value");

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);

        let db = Arc::new(db::AiDb::open(None).unwrap());
        let cfg = Arc::new(config::AiConfig::default());
        tokio::spawn(run_loop(plugin_client, db, cfg));

        for (id, tag) in [("req1", "one"), ("req2", "two")] {
            let env = Envelope {
                payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                    action_id: id.to_string(),
                    action: "chat_completion".to_string(),
                    params_json: chat_params(tag),
                    timeout_ms: 5000,
                    streaming: false,
                    ..Default::default()
                })),
                ..Default::default()
            };
            kernel.send("client", env).await.unwrap();
        }

        // Both handler tasks must reach their outbound http_request call
        // before either is answered — sequential dispatch would only ever
        // show the second one after the first's full round trip completed.
        let reqs = collect_http_requests(&mut kernel, 2).await;
        let (first, second) = (&reqs[0], &reqs[1]);
        assert_ne!(first.action_id, second.action_id);

        // Both provider calls are still unanswered right now — this is
        // exactly the state that used to starve the watchdog's Ping.
        assert_ping_answered_promptly(&mut kernel).await;

        for req in [first, second] {
            let net = serde_json::json!({"status": 200, "body": ok_completion_body(), "body_encoding": ""});
            let resp = Envelope {
                payload: Some(envelope::Payload::ActionResponse(ActionResponse {
                    action_id: req.action_id.clone(),
                    status: ActionStatus::ActionOk as i32,
                    data_json: serde_json::to_vec(&net).unwrap(),
                    error: String::new(),
                })),
                ..Default::default()
            };
            kernel.send("client", resp).await.unwrap();
        }

        let mut got = HashSet::new();
        for _ in 0..2 {
            let resp = recv_action_response(&mut kernel).await;
            assert_eq!(resp.status, ActionStatus::ActionOk as i32, "error: {}", resp.error);
            got.insert(resp.action_id);
        }
        assert_eq!(got, ["req1".to_string(), "req2".to_string()].into_iter().collect());
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
