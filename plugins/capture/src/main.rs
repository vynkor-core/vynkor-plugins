mod handler_stub {}

use std::sync::Arc;

use serde_json::Value;
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, PluginManifest,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "capture";
const PLUGIN_VERSION: &str = "0.1.0";

struct App;

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_SCREEN".to_string()],
        actions: vec!["capture_status".to_string()],
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
            Err(_) => break,
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
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

async fn handle_action_request(_app: &App, req: ActionRequest) -> ActionResponse {
    let result: Result<Value, String> = match req.action.as_str() {
        "capture_status" => Ok(serde_json::json!({
            "session_type": "unknown",
            "screenshot_backend": null,
            "record_backend": null,
            "ocr_available": false,
            "portal_available": false
        })),
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
        Err(error) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionError as i32,
            data_json: Vec::new(),
            error,
        },
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
    let app = Arc::new(App);
    let client = VynkorClient::connect_from_env().await?;
    serve(client, app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
    use vynkor_sdk::proto::PluginRegisterAck;

    type Replies = Arc<AsyncMutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

    enum Cmd {
        Call { action: String, params: Value, reply: oneshot::Sender<Result<Value, String>> },
    }

    struct Shim {
        tx: mpsc::Sender<Cmd>,
    }

    impl Shim {
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.tx
                .send(Cmd::Call { action: action.to_string(), params, reply: reply_tx })
                .await
                .expect("shim loop died");
            tokio::time::timeout(Duration::from_secs(5), reply_rx)
                .await
                .expect("timed out waiting for plugin reply")
                .expect("shim dropped reply channel")
        }
    }

    async fn start_plugin() -> Shim {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        let app = Arc::new(App);
        tokio::spawn(async move {
            let _ = serve(plugin_client, app).await;
        });

        let (tx, rx) = mpsc::channel::<Cmd>(16);
        let replies: Replies = Arc::new(AsyncMutex::new(HashMap::new()));
        tokio::spawn(run_shim(kernel_client, rx, replies));
        Shim { tx }
    }

    async fn run_shim(mut kernel: VynkorClient, mut rx: mpsc::Receiver<Cmd>, replies: Replies) {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for plugin registration")
                .expect("plugin stream closed before registration");
            if matches!(env.payload, Some(envelope::Payload::PluginRegister(_))) {
                let _ = kernel
                    .send(
                        "capture",
                        Envelope {
                            payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck {
                                accepted: true,
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                    )
                    .await;
                break;
            }
        }

        let mut seq: u64 = 0;
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
                            let _ = kernel.send("capture", Envelope {
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
    async fn e2e_capture_status_round_trip() {
        let shim = start_plugin().await;
        let v = shim.call("capture_status", serde_json::json!({})).await.unwrap();
        assert_eq!(v["ocr_available"], false);
    }

    #[tokio::test]
    async fn e2e_unknown_action_is_not_found() {
        let shim = start_plugin().await;
        let err = shim.call("capture_bogus", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }
}
