//! `sound` plugin — audio output primitive for vynkor plugins.
//!
//! `sound_play` / `sound_stop` / `sound_status`: spawn a host player binary
//! directly with argv (never a shell) and let clips play in the background.
//! Declares `PERMISSION_AUDIO`. Same thin shape as `clipboard`: the serve
//! loop is sequential and makes no outbound calls, so it is the single
//! reader of the connection with no RPC proxy needed.

mod handler;
mod manifest;
mod players;

use std::sync::{Arc, Mutex};

use serde_json::Value;
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, PluginManifest,
};
use vynkor_sdk::{VynkorClient, VynkorError};

use handler::SharedState;
use players::{Config as PlayerConfig, RealSpawner};

const PLUGIN_ID: &str = "sound";
const PLUGIN_VERSION: &str = "0.1.0";

struct App {
    cfg: PlayerConfig,
    spawner: Arc<dyn players::Spawner>,
    state: SharedState,
}

impl App {
    fn new(cfg: PlayerConfig, spawner: Arc<dyn players::Spawner>) -> Self {
        Self {
            cfg,
            spawner,
            state: Arc::new(Mutex::new(handler::State::new())),
        }
    }
}

fn manifest() -> PluginManifest {
    PluginManifest {
        actions: vec![
            "sound_play".to_string(),
            "sound_stop".to_string(),
            "sound_status".to_string(),
            "sound_devices".to_string(),
        ],
        action_specs: manifest::action_specs(),
        ..Default::default()
    }
}

async fn serve(mut client: VynkorClient, app: Arc<App>) -> Result<(), VynkorError> {
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
        let env = match client.recv().await {
            Ok(env) => env,
            Err(_) => break, // disconnect / EOF
        };
        match env.payload {
            Some(envelope::Payload::Ping(ping)) => {
                let pong = Envelope {
                    payload: Some(envelope::Payload::Pong(vynkor_sdk::proto::Pong {
                        original_timestamp: ping.timestamp,
                        server_timestamp: unix_millis(),
                    })),
                    ..Default::default()
                };
                let _ = client.send("kernel", pong).await;
            }
            Some(envelope::Payload::PluginShutdown(_)) => break,
            Some(envelope::Payload::Event(event)) => {
                // sound declares no event subscriptions; ack defensively so
                // the kernel doesn't retry anything unexpectedly delivered.
                let _ = client.ack_event(&event.event_id).await;
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
            }
            _ => {}
        }
    }

    // Best-effort: don't leave clips playing after the plugin is gone.
    let stopped = handler::kill_all(&app.state);
    if !stopped.is_empty() {
        println!(
            "[{PLUGIN_ID}] stopped {} clip(s) on shutdown",
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
        "sound_play" => match handler::PlayRequest::parse(&params) {
            Ok(parsed) => {
                handler::handle_play(app.spawner.as_ref(), &app.cfg, &app.state, &parsed).await
            }
            Err(e) => Err(e),
        },
        "sound_stop" => {
            let clip_id = params.get("clip_id").and_then(Value::as_str);
            Ok(handler::handle_stop(&app.state, clip_id))
        }
        "sound_status" => Ok(handler::handle_status(&app.state)),
        "sound_devices" => handler::handle_devices().await,
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
    let app = Arc::new(App::new(PlayerConfig::from_env(), Arc::new(RealSpawner)));
    let client = VynkorClient::connect_from_env().await?;
    serve(client, app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use players::FakeSpawner;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
    use vynkor_sdk::proto::PluginRegisterAck;

    const SILENT_WAV_B64: &str = "UklGRiQAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA=";

    type Replies = Arc<AsyncMutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

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
    async fn start_plugin(spawner: Arc<dyn players::Spawner>) -> Shim {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        let app = Arc::new(App::new(test_cfg(), spawner));
        tokio::spawn(async move {
            let _ = serve(plugin_client, app).await;
        });

        let (tx, rx) = mpsc::channel::<Cmd>(16);
        let replies: Replies = Arc::new(AsyncMutex::new(HashMap::new()));
        tokio::spawn(run_shim(kernel_client, rx, replies));
        Shim { tx }
    }

    fn test_cfg() -> PlayerConfig {
        PlayerConfig {
            max_bytes: 1024 * 1024,
            player_override: None,
            default_device: None,
            temp_dir: std::env::temp_dir(),
        }
    }

    async fn run_shim(mut kernel: VynkorClient, mut rx: mpsc::Receiver<Cmd>, replies: Replies) {
        let mut seq: u64 = 0;
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for plugin registration")
                .expect("plugin stream closed before registration");
            if matches!(env.payload, Some(envelope::Payload::PluginRegister(_))) {
                let _ = kernel
                    .send(
                        "sound",
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
                    if let Some(envelope::Payload::ActionResponse(resp)) = env.payload {
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
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Call { action, params, reply }) => {
                            seq += 1;
                            let action_id = format!("t-{seq}");
                            replies.lock().await.insert(action_id.clone(), reply);
                            let _ = kernel.send("sound", Envelope {
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
    #[tokio::test]
    async fn e2e_play_status_stop_over_wire() {
        let sp = FakeSpawner::ok(None);
        let shim = start_plugin(Arc::new(sp)).await;

        let v = shim
            .call(
                "sound_play",
                serde_json::json!({
                    "data_base64": SILENT_WAV_B64,
                    "format": "wav"
                }),
            )
            .await
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["player"], "pw-cat");
        let clip_id = v["clip_id"].as_str().unwrap().to_string();

        let v = shim
            .call("sound_status", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["playing"][0]["id"], clip_id.as_str());

        let v = shim
            .call("sound_stop", serde_json::json!({ "clip_id": clip_id }))
            .await
            .unwrap();
        assert_eq!(v["stopped"], serde_json::json!([clip_id]));

        let v = shim
            .call("sound_status", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(v["count"], 0);
    }

    #[tokio::test]
    async fn e2e_bad_params_become_action_error() {
        let shim = start_plugin(Arc::new(FakeSpawner::ok(None))).await;

        let err = shim
            .call("sound_play", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("ERR_SOUND_BAD_PARAMS"), "{err}");

        let err = shim
            .call(
                "sound_play",
                serde_json::json!({ "file": "/a.wav", "data_base64": "x", "format": "wav" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
    }

    #[tokio::test]
    async fn e2e_unknown_action_is_not_found() {
        let shim = start_plugin(Arc::new(FakeSpawner::ok(None))).await;
        let err = shim
            .call("sound_volume_set", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }

    #[tokio::test]
    async fn dispatch_rejects_malformed_params_json() {
        let app = App::new(test_cfg(), Arc::new(FakeSpawner::ok(None)));
        let resp = handle_action_request(
            &app,
            ActionRequest {
                action_id: "x".into(),
                action: "sound_play".into(),
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
    fn silent_wav_fixture_is_valid_base64() {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(SILENT_WAV_B64)
            .unwrap();
        assert!(decoded.starts_with(b"RIFF"));
    }
}
