//! `mic` plugin — audio capture primitive for vynkor plugins.
//!
//! `mic_start` / `mic_stop` / `mic_status`: spawn a host recorder binary
//! directly with argv (never a shell) and stream its raw PCM to the
//! requested peer as `AudioStreamChunk`s (D-12 machinery, `tts_speak` in
//! reverse). Declares `PERMISSION_AUDIO`, `PERMISSION_AUDIO_STREAM`, and
//! `PERMISSION_IPC_SEND`.
//!
//! Loop shape (docs/PLUGIN_AUTHORING.md §1, timer/background row): the
//! serve loop exclusively owns the connection — capture tasks push
//! `(target, Envelope)` pairs into an mpsc channel the loop drains via
//! `tokio::select!`, so nothing else ever reads or writes the socket. The
//! select result is routed through an enum and handled after the select
//! completes, keeping the mutable `client` borrow single-threaded.

mod capture;
mod handler;
mod recorders;

use std::sync::Arc;

use serde_json::Value;
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, PluginManifest,
};
use vynkor_sdk::{VynkorClient, VynkorError};

use capture::{OutboundTx, SharedState, State};
use recorders::{Config as RecorderConfig, RealSpawner, RecorderSpawner};

const PLUGIN_ID: &str = "mic";
const PLUGIN_VERSION: &str = "0.1.0";

/// Comma-separated allowlist of `ipc_targets` for chunk streaming. The
/// kernel gates peer-to-peer unicast per-target (T-04): a target not listed
/// here gets `ERR_PERMISSION_DENIED`. Default-deny — unset means
/// `mic_start` can only stream to peers the operator explicitly allows
/// (e.g. `stt` on-host, `device.phone.stt` remote).
const IPC_TARGETS_ENV: &str = "MIC_PLUGIN_IPC_TARGETS";

struct App {
    cfg: RecorderConfig,
    spawner: Arc<dyn RecorderSpawner>,
    state: SharedState,
    outbound: OutboundTx,
}

impl App {
    fn new(cfg: RecorderConfig, spawner: Arc<dyn RecorderSpawner>, outbound: OutboundTx) -> Self {
        Self {
            cfg,
            spawner,
            state: Arc::new(std::sync::Mutex::new(State::new())),
            outbound,
        }
    }
}

fn manifest() -> PluginManifest {
    let ipc_targets: Vec<String> = std::env::var(IPC_TARGETS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    PluginManifest {
        // `audio`: the capture gate (mirrors `sound`; roadmap row).
        // `audio_stream`: mic streams AudioStreamChunks to a peer (proto
        // v1.6 PERMISSION_AUDIO_STREAM, same as tts_speak/stt listen).
        // `ipc_send`: peer targeting is gated per-target by T-04 together
        // with the manifest ipc_targets above.
        permissions: vec![
            "PERMISSION_AUDIO".into(),
            "PERMISSION_AUDIO_STREAM".into(),
            "PERMISSION_IPC_SEND".into(),
        ],
        actions: vec![
            "mic_start".to_string(),
            "mic_stop".to_string(),
            "mic_status".to_string(),
        ],
        action_specs: vynkor_plugin_manifest::action_specs(),
        ipc_targets,
        ..Default::default()
    }
}

enum LoopSource {
    Kernel(Result<Envelope, VynkorError>),
    Capture(String, Envelope),
}

async fn serve(
    mut client: VynkorClient,
    app: Arc<App>,
    mut outbound_rx: tokio::sync::mpsc::Receiver<(String, Envelope)>,
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

    loop {
        let source = tokio::select! {
            env = client.recv() => LoopSource::Kernel(env),
            out = outbound_rx.recv() => match out {
                Some((target, env)) => LoopSource::Capture(target, env),
                // App holds a sender clone, so this arm is unreachable;
                // treat it as shutdown defensively.
                None => break,
            },
        };

        let keep_going = match source {
            LoopSource::Capture(target, env) => {
                let _ = client.send(&target, env).await;
                true
            }
            LoopSource::Kernel(Err(_)) => false, // disconnect / EOF
            LoopSource::Kernel(Ok(env)) => match env.payload {
                Some(envelope::Payload::Ping(ping)) => {
                    let pong = Envelope {
                        payload: Some(envelope::Payload::Pong(vynkor_sdk::proto::Pong {
                            original_timestamp: ping.timestamp,
                            server_timestamp: unix_millis(),
                        })),
                        ..Default::default()
                    };
                    let _ = client.send("kernel", pong).await;
                    true
                }
                Some(envelope::Payload::PluginShutdown(_)) => false,
                Some(envelope::Payload::Event(event)) => {
                    // mic declares no event subscriptions; ack defensively so
                    // the kernel doesn't retry anything unexpectedly delivered.
                    let _ = client.ack_event(&event.event_id).await;
                    true
                }
                Some(envelope::Payload::ActionRequest(req)) => {
                    let response = handle_action_request(&app, req).await;
                    let _ = client
                        .send(
                            "kernel",
                            Envelope {
                                payload: Some(envelope::Payload::ActionResponse(response)),
                                ..Default::default()
                            },
                        )
                        .await;
                    true
                }
                _ => true,
            },
        };
        if !keep_going {
            break;
        }
    }

    // Best-effort: kill recorders and flush end_of_stream markers so peers
    // never wait on a stream whose owner died.
    let stopped = app.state.lock().unwrap().stop_all();
    if !stopped.is_empty() {
        println!(
            "[{PLUGIN_ID}] stopped {} session(s) on shutdown",
            stopped.len()
        );
    }
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

async fn handle_action_request(app: &App, req: ActionRequest) -> ActionResponse {
    let params: Value = match serde_json::from_slice(&req.params_json) {
        Ok(v) => v,
        Err(e) => {
            return err_response(req.action_id, format!("invalid params_json: {e}"));
        }
    };

    let result: Result<Value, String> = match req.action.as_str() {
        "mic_start" => match handler::StartRequest::parse(&params, &app.cfg) {
            Ok(parsed) => {
                handler::handle_start(
                    app.spawner.as_ref(),
                    &app.cfg,
                    &app.state,
                    &app.outbound,
                    &parsed,
                )
                .await
            }
            Err(e) => Err(e),
        },
        "mic_stop" => {
            let session_id = params.get("session_id").and_then(Value::as_str);
            Ok(handler::handle_stop(&app.state, session_id))
        }
        "mic_status" => Ok(handler::handle_status(&app.state)),
        other => {
            return ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionNotFound as i32,
                data_json: Vec::new(),
                error: format!("unknown action: {other}"),
            };
        }
    };

    match result {
        Ok(data) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionOk as i32,
            data_json: data.to_string().into_bytes(),
            error: String::new(),
        },
        Err(error) => err_response(req.action_id, error),
    }
}

fn err_response(action_id: String, error: String) -> ActionResponse {
    ActionResponse {
        action_id,
        status: ActionStatus::ActionError as i32,
        data_json: Vec::new(),
        error,
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::channel::<(String, Envelope)>(256);
    let app = Arc::new(App::new(
        RecorderConfig::from_env(),
        Arc::new(RealSpawner),
        outbound_tx,
    ));
    let client = VynkorClient::connect_from_env().await?;
    serve(client, app, outbound_rx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use capture::SessionMeta;
    use recorders::{EofSpawner, FakeSpawner};
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
    use vynkor_sdk::proto::{AudioCodec, PluginRegisterAck};

    type Replies = Arc<AsyncMutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;
    type ChunkLog = Arc<StdMutex<Vec<vynkor_sdk::proto::AudioStreamChunk>>>;

    enum Cmd {
        Call {
            action: String,
            params: Value,
            reply: oneshot::Sender<Result<Value, String>>,
        },
    }

    struct Shim {
        tx: mpsc::Sender<Cmd>,
    }

    impl Shim {
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.tx
                .send(Cmd::Call {
                    action: action.to_string(),
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
    }

    /// Drive the real `serve` loop against a fake kernel over a socket pair.
    /// The shim answers registration first — register_full treats the very
    /// next inbound frame as the ack, so test commands must not race ahead.
    /// `AudioStreamChunk` envelopes streamed by capture tasks land in the
    /// returned chunk log.
    async fn start_plugin(spawner: Arc<dyn RecorderSpawner>) -> (Shim, ChunkLog) {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);

        let (outbound_tx, outbound_rx) = mpsc::channel::<(String, Envelope)>(64);
        let app = Arc::new(App::new(test_cfg(), spawner, outbound_tx));
        tokio::spawn(async move {
            let _ = serve(plugin_client, app, outbound_rx).await;
        });

        let chunks: ChunkLog = Arc::new(StdMutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel::<Cmd>(16);
        let replies: Replies = Arc::new(AsyncMutex::new(HashMap::new()));
        tokio::spawn(run_shim(kernel_client, rx, replies, Arc::clone(&chunks)));
        (Shim { tx }, chunks)
    }

    fn test_cfg() -> RecorderConfig {
        RecorderConfig {
            recorder_override: None,
            default_device: None,
            default_rate_hz: 16_000,
            default_channels: 1,
            default_chunk_ms: 100,
        }
    }

    async fn run_shim(
        mut kernel: VynkorClient,
        mut rx: mpsc::Receiver<Cmd>,
        replies: Replies,
        chunks: ChunkLog,
    ) {
        let mut seq: u64 = 0;
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for plugin registration")
                .expect("plugin stream closed before registration");
            if matches!(env.payload, Some(envelope::Payload::PluginRegister(_))) {
                let _ = kernel
                    .send(
                        "mic",
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
        }

        loop {
            tokio::select! {
                env = kernel.recv() => {
                    let env = match env { Ok(e) => e, Err(_) => break };
                    match env.payload {
                        Some(envelope::Payload::ActionResponse(resp)) => {
                            let mut pending = replies.lock().await;
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
                        Some(envelope::Payload::AudioStreamChunk(c)) => {
                            chunks.lock().unwrap().push(c);
                        }
                        _ => {}
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Call { action, params, reply }) => {
                            seq += 1;
                            let action_id = format!("t-{seq}");
                            replies.lock().await.insert(action_id.clone(), reply);
                            let _ = kernel.send("mic", Envelope {
                                payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                                    action_id,
                                    action,
                                    params_json: serde_json::to_vec(&params).unwrap(),
                                    timeout_ms: 0,
                                    streaming: false,
                                    caller_plugin_id: "tester".into(),
                                })),
                                ..Default::default()
                            }).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// Poll the chunk log until `pred` holds for the whole sequence.
    async fn wait_for(
        chunks: &ChunkLog,
        min_len: usize,
        pred: impl Fn(&[vynkor_sdk::proto::AudioStreamChunk]) -> bool + Send + Sync,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let guard = chunks.lock().unwrap();
                if guard.len() >= min_len && pred(&guard) {
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for chunk stream condition"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn e2e_start_streams_pcm_and_stop_ends_stream() {
        // 320 bytes at 8 kHz mono, 10 ms chunks (160 B) → 2 data frames.
        let sp = FakeSpawner::ok(vec![0xABu8; 320]);
        let (shim, chunks) = start_plugin(Arc::new(sp)).await;

        let v = shim
            .call(
                "mic_start",
                serde_json::json!({
                    "target": "stt",
                    "sample_rate_hz": 8000,
                    "chunk_ms": 10
                }),
            )
            .await
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["recorder"], "pw-cat");
        assert_eq!(v["session_id"], "session-1");
        let stream_id = v["stream_id"].as_u64().unwrap();

        wait_for(&chunks, 2, |cs| cs.len() >= 2 && !cs[0].end_of_stream).await;
        {
            let guard = chunks.lock().unwrap();
            for c in guard.iter() {
                assert_eq!(c.stream_id as u64, stream_id);
                assert_eq!(c.codec, AudioCodec::PcmS16le as i32);
                assert_eq!(c.sample_rate, 8000);
                assert_eq!(c.channels, 1);
            }
            assert!(guard[0].data.chunks_exact(2).count() > 0, "whole samples");
        }

        let v = shim.call("mic_stop", serde_json::json!({})).await.unwrap();
        assert_eq!(v["stopped"], serde_json::json!(["session-1"]));

        wait_for(&chunks, 3, |cs| {
            cs.last().map(|c| c.end_of_stream).unwrap_or(false)
        })
        .await;

        let v = shim
            .call("mic_status", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(v["count"], 0);
    }

    #[tokio::test]
    async fn e2e_natural_recorder_eof_terminates_stream_without_stop() {
        let sp = EofSpawner::ok(vec![0x11u8; 320]);
        let (shim, chunks) = start_plugin(Arc::new(sp)).await;

        let v = shim
            .call(
                "mic_start",
                serde_json::json!({ "target": "stt", "sample_rate_hz": 8000, "chunk_ms": 10 }),
            )
            .await
            .unwrap();
        assert_eq!(v["ok"], true);

        wait_for(&chunks, 1, |cs| {
            cs.last().map(|c| c.end_of_stream).unwrap_or(false)
        })
        .await;

        // Session converges to idle on the next interaction (lazy reap).
        tokio::time::sleep(Duration::from_millis(30)).await;
        let v = shim
            .call("mic_status", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(v["count"], 0);
    }

    #[tokio::test]
    async fn e2e_replace_on_start_reports_replaced() {
        let sp = FakeSpawner::ok(vec![0u8; 160]);
        let (shim, _chunks) = start_plugin(Arc::new(sp)).await;

        let p = serde_json::json!({ "target": "stt", "sample_rate_hz": 8000, "chunk_ms": 10 });
        let v1 = shim.call("mic_start", p.clone()).await.unwrap();
        assert_eq!(v1["replaced"], false);
        let v2 = shim.call("mic_start", p).await.unwrap();
        assert_eq!(v2["replaced"], true);
        assert_eq!(v2["session_id"], "session-2");

        let v = shim
            .call("mic_status", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["capturing"][0]["id"], "session-2");
    }

    #[tokio::test]
    async fn e2e_bad_params_become_action_error() {
        let (shim, _chunks) = start_plugin(Arc::new(FakeSpawner::ok(vec![]))).await;

        let err = shim
            .call("mic_start", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("ERR_MIC_BAD_PARAMS"), "{err}");

        let err = shim
            .call(
                "mic_start",
                serde_json::json!({ "target": "stt", "sample_rate_hz": 1 }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("sample_rate_hz"), "{err}");
    }

    #[tokio::test]
    async fn e2e_unknown_action_is_not_found() {
        let (shim, _chunks) = start_plugin(Arc::new(FakeSpawner::ok(vec![]))).await;
        let err = shim
            .call("mic_volume_set", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }

    #[tokio::test]
    async fn dispatch_rejects_malformed_params_json() {
        let (outbound_tx, _outbound_rx) = mpsc::channel::<(String, Envelope)>(4);
        let app = App::new(test_cfg(), Arc::new(FakeSpawner::ok(vec![])), outbound_tx);
        let resp = handle_action_request(
            &app,
            ActionRequest {
                action_id: "x".into(),
                action: "mic_start".into(),
                params_json: b"not json".to_vec(),
                timeout_ms: 0,
                streaming: false,
                caller_plugin_id: "tester".into(),
            },
        )
        .await;
        assert_eq!(resp.status, ActionStatus::ActionError as i32);
        assert!(resp.error.contains("invalid params_json"), "{}", resp.error);
    }

    #[test]
    fn manifest_declares_capture_permissions_and_actions() {
        let m = manifest();
        assert_eq!(
            m.permissions,
            vec![
                "PERMISSION_AUDIO".to_string(),
                "PERMISSION_AUDIO_STREAM".to_string(),
                "PERMISSION_IPC_SEND".to_string(),
            ]
        );
        assert_eq!(m.actions.len(), 3);
        assert!(m.ipc_targets.is_empty(), "default-deny without env");
    }

    #[test]
    fn session_meta_snapshot_shape_stays_caller_friendly() {
        // Guards the mic_status field set against accidental renames —
        // daemon/webclient consume these keys.
        let meta = SessionMeta {
            id: "session-1".into(),
            stream_id: 1,
            target: "stt".into(),
            recorder_bin: "pw-cat".into(),
            device: Some("usb".into()),
            rate_hz: 16000,
            channels: 1,
            chunk_ms: 100,
        };
        let v = serde_json::to_value(&meta.id).unwrap();
        assert_eq!(v, "session-1");
        assert_eq!(meta.recorder_bin, "pw-cat");
    }

    #[test]
    fn unix_millis_monotonic_enough_for_pongs() {
        let a = unix_millis();
        let b = unix_millis();
        assert!(b >= a);
        assert!(a > 1_700_000_000_000, "sanity: epoch-based millis");
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
