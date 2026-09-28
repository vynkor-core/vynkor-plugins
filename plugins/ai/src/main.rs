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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ai_plugin::outbound::{ActionCaller, Outbound, OutboundHandle, UpstreamEvent};
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
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

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
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Outbound>(CHANNEL_CAPACITY);
    // In-flight outbound calls this loop has sent to the kernel on a
    // handler's behalf, keyed by the `action_id` it minted, waiting for the
    // matching `ActionResponse` to route back through the oneshot.
    let mut pending: HashMap<
        String,
        (
            oneshot::Sender<Result<ActionResponse, VynkorError>>,
            Instant,
        ),
    > = HashMap::new();
    let mut sweep = tokio::time::interval(PENDING_SWEEP_INTERVAL);
    // CD-03: outbound streaming sessions (ours → network), keyed by the id
    // we minted — the kernel addresses their frames back by that id.
    let mut up_streams: HashMap<String, mpsc::UnboundedSender<UpstreamEvent>> = HashMap::new();
    // CD-03: inbound streaming chat_completions (caller → us), keyed by the
    // kernel-internal id the caller's SessionClose will name. Shared with
    // the stream tasks so they can drop their own entry on exit.
    let inbound_streams: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

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
                    Some(envelope::Payload::ActionRequest(req)) if req.streaming => {
                        if req.action != "chat_completion" {
                            // accepting would open a kernel session this
                            // action never feeds or closes
                            let reply = Envelope {
                                payload: Some(envelope::Payload::ActionResponse(ActionResponse {
                                    action_id: req.action_id,
                                    status: ActionStatus::ActionError as i32,
                                    data_json: Vec::new(),
                                    error: format!("{} does not support streaming", req.action),
                                })),
                                ..Default::default()
                            };
                            let _ = client.send("kernel", reply).await;
                            continue;
                        }
                        let (cancel_tx, cancel_rx) = oneshot::channel();
                        inbound_streams
                            .lock()
                            .unwrap()
                            .insert(req.action_id.clone(), cancel_tx);
                        let db = db.clone();
                        let resp_tx = resp_tx.clone();
                        let streams = inbound_streams.clone();
                        let mut caller = OutboundHandle::new(outbound_tx.clone());
                        tokio::spawn(async move {
                            let outcome = handler::handle_chat_completion_stream(
                                &mut caller,
                                &req.action_id,
                                &req.params_json,
                                &db,
                                &resp_tx,
                                cancel_rx,
                            )
                            .await;
                            streams.lock().unwrap().remove(&req.action_id);
                            if let Err(error) = outcome {
                                let reply = Envelope {
                                    payload: Some(envelope::Payload::ActionResponse(ActionResponse {
                                        action_id: req.action_id,
                                        status: ActionStatus::ActionError as i32,
                                        data_json: Vec::new(),
                                        error,
                                    })),
                                    ..Default::default()
                                };
                                let _ = resp_tx.send(reply).await;
                            }
                        });
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
                        } else if resp.status == ActionStatus::ActionOk as i32
                            && up_streams.contains_key(&resp.action_id)
                        {
                            // acceptance: the session stays open
                            let _ = up_streams[&resp.action_id]
                                .send(UpstreamEvent::Accepted(resp.data_json));
                        } else if let Some(events) = up_streams.remove(&resp.action_id) {
                            let _ = events.send(UpstreamEvent::Failed(resp.error));
                        } else {
                            eprintln!(
                                "[{PLUGIN_ID}] stray ActionResponse for unknown action_id {}",
                                resp.action_id
                            );
                        }
                    }
                    Some(envelope::Payload::ActionResponseChunk(chunk)) => {
                        if let Some(events) = up_streams.get(&chunk.action_id) {
                            let _ = events.send(UpstreamEvent::Chunk(chunk.chunk));
                        }
                    }
                    Some(envelope::Payload::SessionClose(close)) => {
                        // ours ending normally, or a caller stopping theirs
                        if let Some(events) = up_streams.remove(&close.action_id) {
                            let _ = events.send(UpstreamEvent::Done);
                        } else if let Some(cancel) =
                            inbound_streams.lock().unwrap().remove(&close.action_id)
                        {
                            let _ = cancel.send(());
                        }
                    }
                    Some(envelope::Payload::ActionStreamAbort(abort)) => {
                        if let Some(events) = up_streams.remove(&abort.action_id) {
                            let _ = events.send(UpstreamEvent::Failed(abort.reason));
                        } else if let Some(cancel) =
                            inbound_streams.lock().unwrap().remove(&abort.action_id)
                        {
                            let _ = cancel.send(());
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
            Some(msg) = outbound_rx.recv() => match msg {
                Outbound::Call(call) => {
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
                Outbound::OpenStream { action, params_json, timeout_ms, events, opened } => {
                    let action_id = uuid::Uuid::new_v4().to_string();
                    let env = Envelope {
                        payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                            action_id: action_id.clone(),
                            action,
                            params_json,
                            timeout_ms,
                            streaming: true,
                            ..Default::default()
                        })),
                        ..Default::default()
                    };
                    up_streams.insert(action_id.clone(), events);
                    match client.send("kernel", env).await {
                        Ok(()) => {
                            let _ = opened.send(Ok(action_id));
                        }
                        Err(e) => {
                            up_streams.remove(&action_id);
                            let _ = opened.send(Err(e));
                        }
                    }
                }
                Outbound::CloseStream { action_id } => {
                    if up_streams.remove(&action_id).is_some() {
                        let _ = client.close_session(&action_id, "caller closed").await;
                    }
                }
            },
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
            assert_eq!(
                resp.status,
                ActionStatus::ActionOk as i32,
                "error: {}",
                resp.error
            );
            got.insert(resp.action_id);
        }
        assert_eq!(
            got,
            ["req1".to_string(), "req2".to_string()]
                .into_iter()
                .collect()
        );
    }
    // ── CD-03: streamed chat_completion ──────────────────────────────

    fn envelope_of(payload: envelope::Payload) -> Envelope {
        Envelope {
            payload: Some(payload),
            ..Default::default()
        }
    }

    fn sse(v: serde_json::Value) -> String {
        format!("data: {v}\n\n")
    }

    /// Next frame for inbound session `id` (acceptance, chunk, close or
    /// error), skipping Pongs and other traffic.
    async fn next_session_frame(kernel: &mut VynkorClient, id: &str) -> envelope::Payload {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for a session frame")
                .unwrap();
            match env.payload {
                Some(envelope::Payload::ActionResponse(r)) if r.action_id == id => {
                    return envelope::Payload::ActionResponse(r)
                }
                Some(envelope::Payload::ActionResponseChunk(c)) if c.action_id == id => {
                    return envelope::Payload::ActionResponseChunk(c)
                }
                Some(envelope::Payload::SessionClose(c)) if c.action_id == id => {
                    return envelope::Payload::SessionClose(c)
                }
                _ => {}
            }
        }
    }

    async fn start_streaming_chat(kernel: &mut VynkorClient, id: &str) -> ActionRequest {
        std::env::set_var(ai_plugin::request::ALLOWED_KEY_ENVS_ENV, "TEST_KEY");
        std::env::set_var("TEST_KEY", "test-key-value");
        let env = envelope_of(envelope::Payload::ActionRequest(ActionRequest {
            action_id: id.to_string(),
            action: "chat_completion".to_string(),
            params_json: chat_params("stream me"),
            timeout_ms: 5000,
            streaming: true,
            ..Default::default()
        }));
        kernel.send("client", env).await.unwrap();
        let up = collect_http_requests(kernel, 1).await.remove(0);
        assert!(
            up.streaming,
            "ai must ask network for a streaming http_request"
        );
        let params: serde_json::Value = serde_json::from_slice(&up.params_json).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(params["body"].as_str().unwrap()).unwrap();
        assert_eq!(body["stream"], true);
        // network accepts with the response head
        let head = serde_json::json!({"status": 200, "headers": {}});
        kernel
            .send(
                "client",
                envelope_of(envelope::Payload::ActionResponse(ActionResponse {
                    action_id: up.action_id.clone(),
                    status: ActionStatus::ActionOk as i32,
                    data_json: serde_json::to_vec(&head).unwrap(),
                    error: String::new(),
                })),
            )
            .await
            .unwrap();
        up
    }

    async fn upstream_chunk(kernel: &mut VynkorClient, up: &ActionRequest, seq: u32, bytes: &str) {
        kernel
            .send(
                "client",
                envelope_of(envelope::Payload::ActionResponseChunk(
                    vynkor_sdk::proto::ActionResponseChunk {
                        action_id: up.action_id.clone(),
                        seq,
                        chunk: bytes.as_bytes().to_vec(),
                    },
                )),
            )
            .await
            .unwrap();
    }

    fn delta_text(p: envelope::Payload) -> serde_json::Value {
        match p {
            envelope::Payload::ActionResponseChunk(c) => serde_json::from_slice(&c.chunk).unwrap(),
            other => panic!("expected a chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn streamed_chat_completion_forwards_deltas_then_result_then_close() {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        let db = Arc::new(db::AiDb::open(None).unwrap());
        tokio::spawn(run_loop(
            VynkorClient::from_stream(plugin_side, None),
            db,
            Arc::new(config::AiConfig::default()),
        ));

        let up = start_streaming_chat(&mut kernel, "s1").await;
        match next_session_frame(&mut kernel, "s1").await {
            envelope::Payload::ActionResponse(r) => {
                assert_eq!(r.status, ActionStatus::ActionOk as i32, "{}", r.error);
            }
            other => panic!("expected acceptance, got {other:?}"),
        }

        // one SSE event split across two network chunks
        let first =
            sse(serde_json::json!({"choices": [{"index": 0, "delta": {"content": "Hel"}}]}));
        let (a, b) = first.split_at(20);
        upstream_chunk(&mut kernel, &up, 0, a).await;
        upstream_chunk(&mut kernel, &up, 1, b).await;
        let rest = sse(
            serde_json::json!({"choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": "stop"}]}),
        ) + &sse(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2}}),
        ) + "data: [DONE]\n\n";
        upstream_chunk(&mut kernel, &up, 2, &rest).await;
        kernel
            .send(
                "client",
                envelope_of(envelope::Payload::SessionClose(
                    vynkor_sdk::proto::SessionClose {
                        action_id: up.action_id.clone(),
                        reason: "done".into(),
                    },
                )),
            )
            .await
            .unwrap();

        assert_eq!(
            delta_text(next_session_frame(&mut kernel, "s1").await)["text"],
            "Hel"
        );
        assert_eq!(
            delta_text(next_session_frame(&mut kernel, "s1").await)["text"],
            "lo"
        );
        let done = delta_text(next_session_frame(&mut kernel, "s1").await);
        assert_eq!(done["type"], "done");
        assert_eq!(done["result"]["content"], "Hello");
        assert_eq!(done["result"]["stop_reason"], "stop");
        assert_eq!(done["result"]["usage"]["output_tokens"], 2);
        match next_session_frame(&mut kernel, "s1").await {
            envelope::Payload::SessionClose(c) => assert_eq!(c.reason, "done"),
            other => panic!("expected SessionClose, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn caller_cancel_closes_the_upstream_session() {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        tokio::spawn(run_loop(
            VynkorClient::from_stream(plugin_side, None),
            Arc::new(db::AiDb::open(None).unwrap()),
            Arc::new(config::AiConfig::default()),
        ));

        let up = start_streaming_chat(&mut kernel, "s2").await;
        assert!(matches!(
            next_session_frame(&mut kernel, "s2").await,
            envelope::Payload::ActionResponse(_)
        ));
        upstream_chunk(
            &mut kernel,
            &up,
            0,
            &sse(serde_json::json!({"choices": [{"index": 0, "delta": {"content": "a"}}]})),
        )
        .await;
        assert_eq!(
            delta_text(next_session_frame(&mut kernel, "s2").await)["text"],
            "a"
        );

        // the user pressed stop: the kernel forwards the caller's close
        kernel
            .send(
                "client",
                envelope_of(envelope::Payload::SessionClose(
                    vynkor_sdk::proto::SessionClose {
                        action_id: "s2".into(),
                        reason: "client closed".into(),
                    },
                )),
            )
            .await
            .unwrap();

        // ai must close its own upstream session so network stops the transfer
        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match kernel.recv().await.unwrap().payload {
                    Some(envelope::Payload::SessionClose(c)) if c.action_id == up.action_id => {
                        return c
                    }
                    Some(envelope::Payload::ActionResponseChunk(c)) if c.action_id == "s2" => {
                        panic!("chunk sent on a cancelled session")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("upstream session was not closed after the caller cancelled");
        assert_eq!(closed.action_id, up.action_id);
    }

    #[tokio::test]
    async fn streaming_request_for_a_non_streaming_action_is_rejected() {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        tokio::spawn(run_loop(
            VynkorClient::from_stream(plugin_side, None),
            Arc::new(db::AiDb::open(None).unwrap()),
            Arc::new(config::AiConfig::default()),
        ));
        kernel
            .send(
                "client",
                envelope_of(envelope::Payload::ActionRequest(ActionRequest {
                    action_id: "s3".into(),
                    action: "list_models".into(),
                    streaming: true,
                    ..Default::default()
                })),
            )
            .await
            .unwrap();
        match next_session_frame(&mut kernel, "s3").await {
            envelope::Payload::ActionResponse(r) => {
                assert_eq!(r.status, ActionStatus::ActionError as i32);
                assert!(r.error.contains("does not support streaming"));
            }
            other => panic!("expected rejection, got {other:?}"),
        }
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
        let undocumented: Vec<&str> = specs
            .iter()
            .filter(|s| s.description.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            undocumented.is_empty(),
            "actions missing a description: {undocumented:?}"
        );
        let unrisked: Vec<&str> = specs
            .iter()
            .filter(|s| s.risk == vynkor_sdk::proto::ActionRisk::Unknown as i32)
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            unrisked.is_empty(),
            "actions missing a risk label: {unrisked:?}"
        );
    }
}
