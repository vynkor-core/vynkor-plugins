//! `daemon` plugin binary — headless voice client serve loop.
//!
//! Serve-loop architecture (calendar/sync-client model): the loop task
//! exclusively owns the `VynkorClient` and is the single reader of the
//! connection, so no inbound frame is ever discarded. Action handlers and
//! the background turn loop run as spawned tasks that reach
//! mic/stt/agent/tts/sound through the [`Rpc`] proxy channel; replies and
//! fire-and-forget events flow back through an outbound channel the loop
//! drains. This matters precisely because of the timer: a turn started by a
//! tick must never eat a user request arriving mid-turn (`send_action`'s
//! discard-while-waiting would).
//!
//! The loop is opt-in: unless `DAEMON_PLUGIN_ENABLED=true`, the daemon
//! registers, serves its control actions, and never touches the mic until a
//! caller runs `daemon_enable` (or one-shot `daemon_turn`/`daemon_say`/
//! `daemon_ask`).

use std::collections::HashMap;
use std::sync::Arc;

use daemon_plugin::{
    event_envelope, handle_action, ptt_task, run_voice_turn, Bus, ChangeEvent, Config,
    DaemonState, Rpc, RpcCall,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, Event, PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "daemon";
const PLUGIN_VERSION: &str = "0.2.0";
const ACTIONS: [&str; 6] = [
    "daemon_enable",
    "daemon_disable",
    "daemon_status",
    "daemon_turn",
    "daemon_say",
    "daemon_ask",
];

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec![
            // Caller of the gated `mic_start` / `sound_play` (T-19: the
            // caller of a gated action must hold its permission).
            "PERMISSION_AUDIO".into(),
            // `turn.completed` / `state.changed` publishes.
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

/// Push one kernel event onto the in-process bus for the vad/ptt listen
/// stages. Malformed payloads become `Value::Null` — subscribers filter by
/// event type first, so a broken payload just never matches.
fn forward_event(bus: &Bus, event: &Event) {
    let payload =
        serde_json::from_slice::<Value>(&event.payload_json).unwrap_or(Value::Null);
    bus.send(&event.event_type, payload);
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
    println!(
        "[{PLUGIN_ID}] listen loop {} (mode {}, turn window {} ms, gap {} ms, stt provider '{}')",
        if config.enabled_at_boot { "on" } else { "off (daemon_enable to start)" },
        config.mode.as_str(),
        config.turn_ms,
        config.gap_ms,
        config.stt_target
    );

    // The daemon reacts to other plugins' events: stt's VAD boundaries
    // drive vad-mode turns, hotkey press/release drives ptt turns.
    // Subscribing unconditionally is harmless in window mode — the kernel
    // just delivers a few extra events the bus drops unread.
    let event_types = [
        config.speech_started_event(),
        config.speech_ended_event(),
        daemon_plugin::EV_HOTKEY_PRESSED.to_string(),
        daemon_plugin::EV_HOTKEY_RELEASED.to_string(),
    ];
    if let Err(e) = client.subscribe(event_types.to_vec()).await {
        eprintln!("[{PLUGIN_ID}] subscribe failed (events unavailable): {e}");
    }

    let config = Arc::new(config);
    let state = Arc::new(DaemonState::new(config.enabled_at_boot));
    let bus = Bus::new();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(64);
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCall>(64);
    let rpc = Rpc::new(rpc_tx);

    if config.mode == daemon_plugin::ListenMode::Ptt {
        tokio::spawn(ptt_task(
            rpc.clone(),
            Arc::clone(&state),
            config.as_ref().clone(),
            bus.clone(),
            outbound_tx.clone(),
        ));
    }

    let mut pending: HashMap<String, (String, oneshot::Sender<Result<Value, String>>)> =
        HashMap::new();
    let mut seq: u64 = 0;

    // Sub-50ms periods would panic tokio's interval; Config clamps gap_ms to
    // >= 50 already, so this is belt-and-suspenders.
    let mut interval =
        tokio::time::interval(std::time::Duration::from_millis(config.gap_ms.max(50)));

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
                        forward_event(&bus, &event);
                        let _ = client.ack_event(&event.event_id).await;
                    }
                    Some(envelope::Payload::EventPublishAck(_)) => {
                        // Ack for our own fire-and-forget publishes.
                    }
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let rpc = rpc.clone();
                        let out = outbound_tx.clone();
                        let config = Arc::clone(&config);
                        let state = Arc::clone(&state);
                        let bus = bus.clone();
                        tokio::spawn(async move {
                            match handle_action(
                                rpc,
                                state,
                                &config,
                                &req.action,
                                &req.params_json,
                                Some(&bus),
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
                                        let _ = out.send(event_envelope(&ev)).await;
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
            _ = interval.tick() => {
                // Opt-in background loop: a tick only starts a turn when the
                // operator enabled it and no turn is already running (the mic
                // has one owner). In ptt mode turns are event-driven — the
                // tick stays out of the way. Manual `daemon_turn` claims the
                // same slot.
                if config.mode == daemon_plugin::ListenMode::Ptt {
                    continue;
                }
                if !state.enabled() || !state.try_begin_turn() {
                    continue;
                }
                let rpc = rpc.clone();
                let out = outbound_tx.clone();
                let state = Arc::clone(&state);
                let config = Arc::clone(&config);
                let bus = bus.clone();
                tokio::spawn(async move {
                    let result = run_voice_turn(&rpc, state.clone(), &config, None, Some(&bus))
                        .await;
                    state.end_turn(&result);
                    let ev = ChangeEvent { event_type: "turn.completed", payload: result };
                    let _ = out.send(event_envelope(&ev)).await;
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
    use std::future::Future;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;
    use vynkor_sdk::proto::{EventPublishAck, EventPublishStatus, PluginRegisterAck};

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;
    type Published = Arc<Mutex<Vec<(String, Value)>>>;

    /// Scriptable stand-in for the five downstream plugins (`mic`, `stt`,
    /// `agent`, `tts`, `sound`). Every action request the plugin makes is
    /// recorded in arrival order; per-stage outcomes come from `Script`.
    #[derive(Clone)]
    struct Shim {
        tx: mpsc::Sender<Cmd>,
        calls: Calls,
        published: Published,
    }

    #[derive(Debug)]
    struct Script {
        /// Text returned by the fake `stt_listen_stop`.
        transcript: String,
        /// final_answer returned by the fake `goal_start`.
        answer: String,
        fail_stt: bool,
        fail_mic: bool,
        fail_goal: bool,
        fail_tts: bool,
        fail_sound: bool,
    }

    impl Default for Script {
        fn default() -> Self {
            Self {
                transcript: "what time is it".to_string(),
                answer: "It is noon.".to_string(),
                fail_stt: false,
                fail_mic: false,
                fail_goal: false,
                fail_tts: false,
                fail_sound: false,
            }
        }
    }

    enum Cmd {
        Call { action: String, params: Value, reply: oneshot::Sender<Result<Value, String>> },
        Tweak(fn(&mut Script)),
        InjectEvent { event_type: String, payload: Value },
    }

    impl Shim {
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            self.call_async(action, params).await
        }

        /// Dispatch without awaiting the plugin's reply — for tests that
        /// drive a turn's event feed WHILE the turn is in flight.
        fn call_async(
            &self,
            action: &str,
            params: Value,
        ) -> impl Future<Output = Result<Value, String>> + Send + 'static {
            let (reply_tx, reply_rx) = oneshot::channel();
            let tx = self.tx.clone();
            let action = action.to_string();
            async move {
                tx.send(Cmd::Call { action, params, reply: reply_tx })
                    .await
                    .expect("shim loop died");
                tokio::time::timeout(Duration::from_secs(10), reply_rx)
                    .await
                    .expect("timed out waiting for plugin reply")
                    .expect("shim dropped reply channel")
            }
        }

        /// Deliver one kernel event into the plugin connection, exactly as
        /// the kernel would for a subscribed type.
        async fn inject_event(&self, event_type: &str, payload: Value) {
            self.tx
                .send(Cmd::InjectEvent { event_type: event_type.into(), payload })
                .await
                .expect("shim loop died");
        }

        /// Poll until the plugin has issued `action` at least `n` times —
        /// synchronizes an in-flight turn's progress before injecting the
        /// next stimulus.
        async fn wait_for_calls(&self, action: &str, n: usize) {
            for _ in 0..250 {
                if self.params_of(action).await.len() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("plugin never called {action} ×{n}");
        }

        /// Mutate the failure/transcript script between test phases.
        async fn tweak(&self, f: fn(&mut Script)) {
            self.tx.send(Cmd::Tweak(f)).await.expect("shim loop died");
        }

        async fn names(&self) -> Vec<String> {
            self.calls.lock().await.iter().map(|(a, _)| a.clone()).collect()
        }

        async fn params_of(&self, action: &str) -> Vec<Value> {
            self.calls
                .lock()
                .await
                .iter()
                .filter(|(a, _)| a == action)
                .map(|(_, p)| p.clone())
                .collect()
        }

        async fn published(&self) -> Vec<(String, Value)> {
            self.published.lock().await.clone()
        }
    }

    /// Start the real `serve` loop against a fake kernel over a socket pair.
    async fn start_plugin(config: Config) -> Shim {
        let (calls, published, script, tx, rx) = start_shim_parts();
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        tokio::spawn(run_shim(
            kernel_client,
            rx,
            calls.clone(),
            published.clone(),
            script,
        ));
        tokio::spawn(async move {
            let _ = serve(plugin_client, config).await;
        });
        Shim { tx, calls, published }
    }

    #[allow(clippy::type_complexity)]
    fn start_shim_parts(
    ) -> (Calls, Published, Arc<Mutex<Script>>, mpsc::Sender<Cmd>, mpsc::Receiver<Cmd>) {
        let (tx, rx) = mpsc::channel::<Cmd>(32);
        (
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Script::default())),
            tx,
            rx,
        )
    }

    fn stage_result(req_action: &str, script: &Script, params: Value) -> Result<Value, String> {
        match req_action {
            "stt_listen_start" => {
                if script.fail_stt {
                    return Err("listen stream 7 is already active".into());
                }
                Ok(serde_json::json!({
                    "stream_id": params["stream_id"],
                    "status": "listening"
                }))
            }
            "mic_start" => {
                if script.fail_mic {
                    return Err("ERR_MIC_SPAWN_FAILED: no recorder found".into());
                }
                Ok(serde_json::json!({
                    "ok": true,
                    "session_id": "session-1",
                    "stream_id": params["stream_id"],
                    "target": params["target"],
                    "recorder": "pw-cat",
                    "format": "pcm_s16le",
                    "sample_rate_hz": params["sample_rate_hz"],
                    "num_channels": 1,
                    "chunk_ms": params["chunk_ms"],
                    "replaced": false
                }))
            }
            "mic_stop" => Ok(serde_json::json!({ "stopped": ["session-1"] })),
            "stt_listen_stop" => {
                if script.fail_stt {
                    return Err("no active listen stream 7".into());
                }
                Ok(serde_json::json!({
                    "stream_id": params["stream_id"],
                    "text": script.transcript,
                    "language": "en",
                    "duration_seconds": 0.5,
                    "model": "sherpa:transducer"
                }))
            }
            "goal_start" => {
                if script.fail_goal {
                    return Err("ai transport down".into());
                }
                Ok(serde_json::json!({
                    "id": "7",
                    "status": "completed",
                    "title": params["goal"],
                    "final_answer": script.answer,
                    "error": "",
                    "steps": [],
                    "pending_tool": "",
                    "max_steps": params["max_steps"]
                }))
            }
            "tts_synthesize" => {
                if script.fail_tts {
                    return Err("sherpa model not loaded".into());
                }
                Ok(serde_json::json!({
                    "format": params["format"],
                    "sample_rate_hz": 24000,
                    "num_channels": 1,
                    "duration_seconds": 1.2,
                    "audio_base64": "QUJDREVG"
                }))
            }
            "sound_play" => {
                if script.fail_sound {
                    return Err("ERR_SOUND_PROVIDER_MISSING".into());
                }
                Ok(serde_json::json!({
                    "ok": true,
                    "clip_id": "clip-1",
                    "player": "pw-cat",
                    "replaced": false
                }))
            }
            other => Err(format!("fake kernel: unexpected action {other}")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_shim(
        mut kernel: VynkorClient,
        mut rx: mpsc::Receiver<Cmd>,
        calls: Calls,
        published: Published,
        script: Arc<Mutex<Script>>,
    ) {
        let mut pending: HashMap<String, oneshot::Sender<Result<Value, String>>> =
            HashMap::new();
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
                Some(envelope::Payload::PluginRegister(reg)) => {
                    let perms: Vec<String> =
                        reg.manifest.map(|m| m.permissions).unwrap_or_default();
                    assert!(perms.contains(&"PERMISSION_AUDIO".to_string()));
                    assert!(perms.contains(&"PERMISSION_EVENT_PUBLISH".to_string()));
                    let _ = kernel.send("daemon", Envelope {
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
                            let outcome = {
                                let script = script.lock().await;
                                calls.lock().await.push((req.action.clone(), params.clone()));
                                stage_result(&req.action, &script, params)
                            };
                            let resp = match outcome {
                                Ok(v) => ActionResponse {
                                    action_id: req.action_id,
                                    status: ActionStatus::ActionOk as i32,
                                    data_json: serde_json::to_vec(&v).unwrap(),
                                    error: String::new(),
                                },
                                Err(e) => ActionResponse {
                                    action_id: req.action_id,
                                    status: ActionStatus::ActionError as i32,
                                    data_json: Vec::new(),
                                    error: e,
                                },
                            };
                            let _ = kernel.send("daemon", Envelope {
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
                            let _ = kernel.send("daemon", Envelope {
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
                            let _ = kernel.send("daemon", Envelope {
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
                            let _ = kernel.send("daemon", env).await;
                        }
                        Some(Cmd::Tweak(f)) => {
                            let mut guard = script.lock().await;
                            f(&mut guard);
                        }
                        Some(Cmd::InjectEvent { event_type, payload }) => {
                            seq += 1;
                            let env = Envelope {
                                payload: Some(envelope::Payload::Event(Event {
                                    event_id: format!("ev-{seq}"),
                                    event_type,
                                    payload_json: serde_json::to_vec(&payload).unwrap(),
                                    retry_count: 0,
                                })),
                                ..Default::default()
                            };
                            let _ = kernel.send("daemon", env).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// Poll until `pred` holds on the published list (background activity is
    /// asynchronous — assertions poll instead of checking once).
    async fn wait_for_published(
        shim: &Shim,
        pred: impl Fn(&(String, Value)) -> bool + Copy,
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

    fn fast_config() -> Config {
        Config {
            turn_ms: 100,
            gap_ms: 100,
            timeout_ms: 5_000,
            goal_timeout_ms: 5_000,
            ..Config::default()
        }
    }

    fn vad_config() -> Config {
        Config {
            mode: daemon_plugin::ListenMode::Vad,
            vad_wait_ms: 5_000,
            vad_max_utterance_ms: 2_000,
            ..fast_config()
        }
    }

    fn ptt_config() -> Config {
        Config {
            mode: daemon_plugin::ListenMode::Ptt,
            ptt_binding: "ptt".into(),
            ptt_max_hold_ms: 5_000,
            ..fast_config()
        }
    }


    #[tokio::test]
    async fn say_speaks_via_tts_then_sound() {
        let shim = start_plugin(Config::default()).await;

        let resp = shim
            .call("daemon_say", serde_json::json!({"text": "hello there"}))
            .await
            .unwrap();
        assert_eq!(resp["spoken"], true);
        assert_eq!(resp["clip_id"], "clip-1");
        assert_eq!(resp["player"], "pw-cat");
        assert_eq!(resp["format"], "wav");

        let calls = shim.calls.lock().await.clone();
        let tts_idx =
            calls.iter().position(|(a, _)| a == "tts_synthesize").expect("no tts call");
        let snd_idx =
            calls.iter().position(|(a, _)| a == "sound_play").expect("no sound call");
        assert!(tts_idx < snd_idx, "synthesize must precede play: {calls:?}");

        let synth = &calls[tts_idx].1;
        assert_eq!(synth["provider"], "sherpa");
        assert_eq!(synth["voice"], "af_heart");
        assert_eq!(synth["text"], "hello there");

        let play = &calls[snd_idx].1;
        assert_eq!(play["data_base64"], "QUJDREVG");
        assert_eq!(play["format"], "wav");
    }

    #[tokio::test]
    async fn ask_roundtrips_the_agent_and_speaks_the_answer() {
        let shim = start_plugin(Config::default()).await;

        let resp =
            shim.call("daemon_ask", serde_json::json!({"prompt": "duck chrome"})).await.unwrap();
        assert_eq!(resp["answer"], "It is noon.");
        assert_eq!(resp["goal_id"], "7");
        assert_eq!(resp["goal_status"], "completed");
        assert_eq!(resp["spoken"], true);

        let goals = shim.params_of("goal_start").await;
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[0]["goal"], "duck chrome");
        assert_eq!(goals[0]["max_steps"], 6);
        assert_eq!(shim.params_of("sound_play").await.len(), 1);
    }

    #[tokio::test]
    async fn ask_without_answer_is_not_spoken() {
        let shim = start_plugin(Config::default()).await;
        shim.tweak(|s| s.answer = String::new()).await;

        let resp =
            shim.call("daemon_ask", serde_json::json!({"prompt": "hi"})).await.unwrap();
        assert_eq!(resp["answer"], Value::Null);
        assert_eq!(resp["spoken"], false);
        assert!(shim.params_of("tts_synthesize").await.is_empty());
    }

    #[tokio::test]
    async fn turn_runs_the_full_pipeline_in_order() {
        let shim = start_plugin(fast_config()).await;

        let resp = shim.call("daemon_turn", serde_json::json!({})).await.unwrap();
        assert_eq!(resp["status"], "answered");
        assert_eq!(resp["transcript"], "what time is it");
        assert_eq!(resp["answer"], "It is noon.");
        assert_eq!(resp["spoken"], true);
        assert_eq!(resp["error"], Value::Null);
        assert!(resp["duration_ms"].as_u64().is_some());

        // Relative order of the seven stages; nothing else in between.
        let names = shim.names().await;
        let stages = [
            "stt_listen_start",
            "mic_start",
            "mic_stop",
            "stt_listen_stop",
            "goal_start",
            "tts_synthesize",
            "sound_play",
        ];
        let mut cursor = 0usize;
        for stage in stages {
            let found = names[cursor..].iter().position(|n| n == stage);
            assert!(
                found.is_some(),
                "stage {stage} missing after index {cursor}: {names:?}"
            );
            cursor += found.unwrap() + 1;
        }
        assert_eq!(cursor, names.len(), "unexpected extra calls: {names:?}");

        // The mic points at stt on the negotiated stream id/rate.
        let mics = shim.params_of("mic_start").await;
        assert_eq!(mics[0]["target"], "stt");
        assert_eq!(mics[0]["stream_id"], 7);
        assert_eq!(mics[0]["sample_rate_hz"], 16000);
        let listens = shim.params_of("stt_listen_start").await;
        assert_eq!(listens[0]["stream_id"], 7);
        assert_eq!(listens[0]["sample_rate_hz"], 16000);

        // Turn result lands on the event bus after the response.
        let event = wait_for_published(&shim, |(t, p)| {
            t == "turn.completed" && p["status"] == "answered"
        })
        .await
        .expect("turn.completed event missing");
        assert_eq!(event.1["transcript"], "what time is it");
    }

    #[tokio::test]
    async fn turn_with_text_skips_the_listen_stage() {
        let shim = start_plugin(fast_config()).await;

        let resp = shim
            .call("daemon_turn", serde_json::json!({"text": "summarize my notes"}))
            .await
            .unwrap();
        assert_eq!(resp["status"], "answered");
        assert_eq!(resp["transcript"], "summarize my notes");

        let names = shim.names().await;
        assert!(!names.iter().any(|n| n.starts_with("stt_")), "names: {names:?}");
        assert!(!names.iter().any(|n| n.starts_with("mic_")), "names: {names:?}");
        assert_eq!(shim.params_of("goal_start").await[0]["goal"], "summarize my notes");
    }

    #[tokio::test]
    async fn silent_transcript_ends_the_turn_without_speaking() {
        let shim = start_plugin(fast_config()).await;
        shim.tweak(|s| s.transcript = "   ".to_string()).await;

        let resp = shim.call("daemon_turn", serde_json::json!({})).await.unwrap();
        assert_eq!(resp["status"], "silent");
        assert_eq!(resp["spoken"], false);

        let names = shim.names().await;
        assert!(
            names.contains(&"mic_stop".to_string()),
            "capture must still be stopped: {names:?}"
        );
        assert!(!names.contains(&"goal_start".to_string()), "names: {names:?}");
        assert!(!names.contains(&"tts_synthesize".to_string()), "names: {names:?}");

        let event = wait_for_published(&shim, |(t, p)| {
            t == "turn.completed" && p["status"] == "silent"
        })
        .await;
        assert!(event.is_some(), "silent turn must publish its outcome");
    }

    #[tokio::test]
    async fn stage_failure_lands_in_the_turn_payload_not_an_action_error() {
        let shim = start_plugin(fast_config()).await;
        shim.tweak(|s| s.fail_goal = true).await;

        let resp = shim.call("daemon_turn", serde_json::json!({})).await.unwrap();
        assert_eq!(resp["status"], "error");
        assert!(
            resp["error"].as_str().unwrap().contains("goal_start"),
            "error was: {:?}",
            resp["error"]
        );
        assert_eq!(resp["transcript"], "what time is it");

        // The turn itself succeeded as an operation → ACTION_OK + event.
        let event = wait_for_published(&shim, |(t, p)| {
            t == "turn.completed" && p["status"] == "error"
        })
        .await
        .expect("failed turn must still publish its outcome");
        assert!(event.1["error"].as_str().unwrap().contains("goal_start"));
    }

    #[tokio::test]
    async fn goal_without_prose_reports_error_instead_of_silence() {
        let shim = start_plugin(fast_config()).await;
        shim.tweak(|s| s.answer = String::new()).await;

        let resp = shim.call("daemon_turn", serde_json::json!({})).await.unwrap();
        assert_eq!(resp["status"], "error");
        assert!(
            resp["error"].as_str().unwrap().contains("without an answer"),
            "error was: {:?}",
            resp["error"]
        );
        assert_eq!(resp["spoken"], false);
        assert_eq!(resp["goal_status"], "completed");
    }

    #[tokio::test]
    async fn mic_failure_reports_error_and_skips_transcription() {
        let shim = start_plugin(fast_config()).await;
        shim.tweak(|s| s.fail_mic = true).await;

        let resp = shim.call("daemon_turn", serde_json::json!({})).await.unwrap();
        assert_eq!(resp["status"], "error");
        assert!(resp["error"].as_str().unwrap().contains("mic_start"));

        // The capture failed, so nothing is transcribed — but the half-open
        // stt buffer IS discarded (fail closed), which surfaces as a
        // stt_listen_stop call that returns no text.
        let stops: Vec<Value> = shim.params_of("stt_listen_stop").await;
        assert_eq!(stops.len(), 1, "buffer must be discarded exactly once");
        assert_eq!(stops[0]["stream_id"], 7);
    }

    #[tokio::test]
    async fn vad_mode_turn_ends_on_the_speech_ended_event() {
        let shim = start_plugin(vad_config()).await;

        let shim2 = shim.clone();
        let turn = tokio::spawn(async move {
            shim2.call("daemon_turn", serde_json::json!({})).await
        });
        // Drive the event feed while the turn is in flight: wait for the
        // capture to open, then play speech start + end off the bus.
        shim.wait_for_calls("mic_start", 1).await;
        shim.inject_event(
            &vad_config().speech_started_event(),
            serde_json::json!({"stream_id": 7}),
        )
        .await;
        shim.inject_event(
            &vad_config().speech_ended_event(),
            serde_json::json!({"stream_id": 7, "speech_ms": 900}),
        )
        .await;

        let resp = turn.await.expect("turn task joined").expect("vad turn must reply");
        assert_eq!(resp["status"], "answered");
        assert_eq!(resp["transcript"], "what time is it");
        assert_eq!(resp["spoken"], true);

        let mics = shim.params_of("mic_start").await;
        assert_eq!(mics.len(), 1, "exactly one capture per vad turn");

        let names = shim.names().await;
        for stage in ["stt_listen_start", "mic_start", "mic_stop", "stt_listen_stop"] {
            assert!(names.contains(&stage.to_string()), "{stage} missing: {names:?}");
        }
    }

    #[tokio::test]
    async fn stt_provider_slug_drives_mic_target_and_vad_events() {
        // The merged `speech` plugin serves stt_* under its own slug: the
        // mic must stream to it and its VAD events arrive as plugin.speech.*.
        let cfg = Config { stt_target: "speech".into(), ..vad_config() };
        let started = cfg.speech_started_event();
        let ended = cfg.speech_ended_event();
        assert_eq!(started, "plugin.speech.stt_speech_started");
        assert_eq!(ended, "plugin.speech.stt_speech_ended");
        let shim = start_plugin(cfg).await;

        let shim2 = shim.clone();
        let turn = tokio::spawn(async move {
            shim2.call("daemon_turn", serde_json::json!({})).await
        });
        shim.wait_for_calls("mic_start", 1).await;
        shim.inject_event(&started, serde_json::json!({"stream_id": 7})).await;
        shim.inject_event(&ended, serde_json::json!({"stream_id": 7, "speech_ms": 900}))
            .await;

        let resp = turn.await.expect("turn task joined").expect("vad turn must reply");
        assert_eq!(resp["status"], "answered");
        let mics = shim.params_of("mic_start").await;
        assert_eq!(mics[0]["target"], "speech");
    }

    #[tokio::test]
    async fn vad_mode_turn_times_out_silently_without_speech() {
        let mut cfg = vad_config();
        cfg.vad_wait_ms = 300;
        let shim = start_plugin(cfg).await;
        // No speech on the bus AND an empty transcript from stt — exactly
        // what a real silent window produces.
        shim.tweak(|s| s.transcript = "   ".to_string()).await;

        let resp = shim.call("daemon_turn", serde_json::json!({})).await.unwrap();
        assert_eq!(resp["status"], "silent");

        // The timed-out capture must still be closed cleanly.
        let names = shim.names().await;
        assert!(names.contains(&"mic_stop".to_string()), "names: {names:?}");
        assert!(names.contains(&"stt_listen_stop".to_string()), "names: {names:?}");
        assert!(!names.contains(&"goal_start".to_string()), "names: {names:?}");
    }

    #[tokio::test]
    async fn ptt_turn_runs_between_press_and_release() {
        let shim = start_plugin(ptt_config()).await;
        shim.call("daemon_enable", serde_json::json!({})).await.unwrap();

        shim.inject_event(
            daemon_plugin::EV_HOTKEY_PRESSED,
            serde_json::json!({"binding": "ptt"}),
        )
        .await;
        shim.wait_for_calls("mic_start", 1).await;
        shim.inject_event(
            daemon_plugin::EV_HOTKEY_RELEASED,
            serde_json::json!({"binding": "ptt"}),
        )
        .await;

        let event = wait_for_published(&shim, |(t, p)| {
            t == "turn.completed" && p["status"] == "answered"
        })
        .await
        .expect("ptt turn must publish its outcome");
        assert_eq!(event.1["transcript"], "what time is it");
        assert_eq!(event.1["answer"], "It is noon.");

        // One clean capture cycle, driven purely by events.
        assert_eq!(shim.params_of("mic_start").await.len(), 1);
        assert_eq!(shim.params_of("mic_stop").await.len(), 1);
    }

    #[tokio::test]
    async fn ptt_press_while_disabled_or_busy_is_ignored() {
        let shim = start_plugin(ptt_config()).await;

        // Disabled: a press must not touch the mic at all.
        shim.inject_event(
            daemon_plugin::EV_HOTKEY_PRESSED,
            serde_json::json!({"binding": "ptt"}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            shim.params_of("mic_start").await.is_empty(),
            "disabled daemon must not capture"
        );

        // Enabled, but the binding id doesn't match → still ignored.
        shim.call("daemon_enable", serde_json::json!({})).await.unwrap();
        shim.inject_event(
            daemon_plugin::EV_HOTKEY_PRESSED,
            serde_json::json!({"binding": "other-key"}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(shim.params_of("mic_start").await.is_empty(), "wrong binding must not capture");
    }

    #[tokio::test]
    async fn ptt_stuck_key_auto_releases_at_max_hold() {
        let mut cfg = ptt_config();
        cfg.ptt_max_hold_ms = 400;
        let shim = start_plugin(cfg).await;
        shim.call("daemon_enable", serde_json::json!({})).await.unwrap();

        shim.inject_event(
            daemon_plugin::EV_HOTKEY_PRESSED,
            serde_json::json!({"binding": "ptt"}),
        )
        .await;
        shim.wait_for_calls("mic_start", 1).await;

        // No release ever comes — the cap must close the turn as an error
        // payload (not an action error) and free the busy slot.
        let event = wait_for_published(&shim, |(t, p)| {
            t == "turn.completed" && p["status"] == "error"
        })
        .await
        .expect("max-hold turn must publish its outcome");
        assert!(
            event.1["error"].as_str().unwrap().contains("release"),
            "error was: {:?}",
            event.1["error"]
        );

        // Slot freed: a follow-up manual turn goes through.
        let resp = shim.call("daemon_turn", serde_json::json!({"text": "ping"})).await;
        assert!(resp.is_ok(), "busy slot must be free after max-hold release");
    }

    #[test]
    fn listen_mode_parses_and_renders() {
        use daemon_plugin::ListenMode;
        assert_eq!(ListenMode::parse("window"), Some(ListenMode::Window));
        assert_eq!(ListenMode::parse(" VAD "), Some(ListenMode::Vad));
        assert_eq!(ListenMode::parse("ptt"), Some(ListenMode::Ptt));
        assert_eq!(ListenMode::parse("voice"), None);
        assert_eq!(ListenMode::default().as_str(), "window");
        assert_eq!(ListenMode::Ptt.as_str(), "ptt");
    }

    #[tokio::test]
    async fn say_and_ask_surface_stage_failures_as_action_errors() {
        let shim = start_plugin(Config::default()).await;
        shim.tweak(|s| s.fail_tts = true).await;

        let err =
            shim.call("daemon_say", serde_json::json!({"text": "boom"})).await.unwrap_err();
        assert!(err.contains("tts_synthesize"), "error was: {err}");

        shim.tweak(|s| {
            s.fail_tts = false;
            s.fail_goal = true;
        })
        .await;
        let err =
            shim.call("daemon_ask", serde_json::json!({"prompt": "hi"})).await.unwrap_err();
        assert!(err.contains("goal_start"), "error was: {err}");

        // Sound failing after a good synthesis fails the whole say.
        shim.tweak(|s| {
            s.fail_goal = false;
            s.fail_sound = true;
        })
        .await;
        let err =
            shim.call("daemon_say", serde_json::json!({"text": "boom"})).await.unwrap_err();
        assert!(err.contains("sound_play"), "error was: {err}");
    }

    #[tokio::test]
    async fn validation_errors_are_action_errors_naming_the_field() {
        let shim = start_plugin(Config::default()).await;

        let err =
            shim.call("daemon_say", serde_json::json!({"text": "  "})).await.unwrap_err();
        assert!(err.contains("non-empty text"), "error was: {err}");

        let err = shim.call("daemon_ask", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("prompt"), "error was: {err}");

        let err =
            shim.call("daemon_enable", serde_json::json!({"x": 1})).await.unwrap_err();
        assert!(err.contains("empty params"), "error was: {err}");

        let err =
            shim.call("daemon_frobnicate", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("unknown action"), "error was: {err}");
    }

    #[tokio::test]
    async fn enable_disable_drive_the_background_loop() {
        let shim = start_plugin(fast_config()).await; // boot-disabled

        // Disabled at boot: no ticks may touch the mic.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            shim.params_of("mic_start").await.is_empty(),
            "disabled loop captured audio"
        );

        let resp = shim.call("daemon_enable", serde_json::json!({})).await.unwrap();
        assert_eq!(resp["enabled"], true);

        // Loop ticks every 100ms with a ~100ms window — a completed turn
        // must appear within a couple of seconds.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        loop {
            let status =
                shim.call("daemon_status", serde_json::json!({})).await.unwrap();
            if status["turns_completed"].as_u64().unwrap_or(0) >= 1 {
                assert_eq!(status["enabled"], true);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no background turn completed after enable: {status}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let event = wait_for_published(&shim, |(t, _)| t == "turn.completed").await;
        assert!(event.is_some(), "loop turn must publish turn.completed");

        let changed = wait_for_published(&shim, |(t, _)| t == "state.changed").await;
        assert!(changed.is_some(), "enable must publish state.changed");

        let resp = shim.call("daemon_disable", serde_json::json!({})).await.unwrap();
        assert_eq!(resp["enabled"], false);

        // Let any in-flight turn finish, then confirm quiescence.
        for _ in 0..60 {
            let status =
                shim.call("daemon_status", serde_json::json!({})).await.unwrap();
            if !status["busy"].as_bool().unwrap_or(true) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let done = shim.call("daemon_status", serde_json::json!({})).await.unwrap();
        let turns_at_disable = done["turns_completed"].as_u64().unwrap();

        tokio::time::sleep(Duration::from_millis(800)).await; // > gap + window
        let after = shim.call("daemon_status", serde_json::json!({})).await.unwrap();
        assert_eq!(
            after["turns_completed"].as_u64().unwrap(),
            turns_at_disable,
            "disabled loop kept turning"
        );

        // Both flips were announced.
        let pubs = shim.published().await;
        let flips: Vec<_> = pubs
            .iter()
            .filter(|(t, _)| t == "state.changed")
            .map(|(_, p)| p["enabled"].clone())
            .collect();
        assert!(flips.contains(&serde_json::json!(true)));
        assert!(flips.contains(&serde_json::json!(false)));
    }

    #[tokio::test]
    async fn status_reflects_state_and_last_turn() {
        let shim = start_plugin(fast_config()).await;

        let status = shim.call("daemon_status", serde_json::json!({})).await.unwrap();
        assert_eq!(status["enabled"], false);
        assert_eq!(status["busy"], false);
        assert_eq!(status["turns_completed"], 0);
        assert_eq!(status["last_turn"], Value::Null);

        shim.call("daemon_say", serde_json::json!({"text": "just speak"})).await.unwrap();
        // say is not a turn — counters stay put.
        let status = shim.call("daemon_status", serde_json::json!({})).await.unwrap();
        assert_eq!(status["turns_completed"], 0);

        shim.call("daemon_turn", serde_json::json!({"text": "hello"})).await.unwrap();
        let status = shim.call("daemon_status", serde_json::json!({})).await.unwrap();
        assert_eq!(status["turns_completed"], 1);
        assert_eq!(status["busy"], false);
        let last = status["last_turn"].as_object().expect("last_turn set");
        assert_eq!(last["status"], "answered");
        assert_eq!(last["transcript"], "hello");
    }

    #[tokio::test]
    async fn manual_turn_while_busy_is_rejected() {
        // A long capture window guarantees the first turn is still holding
        // the single slot when the second request lands.
        let slow_cfg = Config { turn_ms: 1_500, gap_ms: 100, ..fast_config() };
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let (calls, published, script, tx, rx) = start_shim_parts();
        tokio::spawn(run_shim(
            VynkorClient::from_stream(kernel_side, None),
            rx,
            calls.clone(),
            published.clone(),
            script,
        ));
        tokio::spawn(async move {
            let _ = serve(VynkorClient::from_stream(plugin_side, None), slow_cfg).await;
        });
        let shim = Shim { tx, calls, published };

        let first = {
            let shim = shim.clone();
            tokio::spawn(async move {
                shim.call("daemon_turn", serde_json::json!({})).await
            })
        };
        tokio::time::sleep(Duration::from_millis(400)).await;
        let err = shim.call("daemon_turn", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("ERR_DAEMON_BUSY"), "error was: {err}");

        let first_result =
            tokio::time::timeout(Duration::from_secs(8), first)
                .await
                .expect("first turn did not finish")
                .is_ok();
        assert!(first_result, "first turn should complete once the window ends");
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
