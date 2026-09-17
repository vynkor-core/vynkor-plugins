//! `capture` plugin — screen capture (screenshot, video record) + local OCR
//! for vynkor plugins. See README.md for the full backend-chain table.
//!
//! ## Concurrency
//!
//! Drives the SDK's concurrent message loop
//! ([`ConcurrentHandler`] + [`serve_concurrent`]) instead of a sequential
//! `recv -> await handler -> reply -> next recv` loop, because
//! `capture_screenshot`'s `xdg-desktop-portal` fallback can block up to 120
//! seconds waiting on a live D-Bus call for a user to interact with an OS
//! dialog — a sequential loop would stall every other concurrent action
//! request (and the kernel's keepalive `Ping`) for that entire window.

use std::sync::Arc;

use capture_plugin::error::CaptureError;
use capture_plugin::record::RecordState;
use capture_plugin::spawner::{RealSpawner, Spawner};
use capture_plugin::{ocr, record, screenshot, session};
use serde_json::Value;
use vynkor_sdk::concurrent::{response_envelope, serve_concurrent};
use vynkor_sdk::proto::{ActionRequest, Envelope, PluginManifest};
use vynkor_sdk::{ConcurrentHandler, VynkorClient, VynkorError};

const PLUGIN_ID: &str = "capture";
const PLUGIN_VERSION: &str = "0.1.0";

struct App {
    spawner: Arc<dyn Spawner>,
    record_state: Arc<RecordState>,
}

impl App {
    fn new() -> Self {
        Self { spawner: Arc::new(RealSpawner), record_state: Arc::new(RecordState::new()) }
    }
}

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_SCREEN".to_string()],
        actions: vec![
            "capture_screenshot".to_string(),
            "capture_record_start".to_string(),
            "capture_record_stop".to_string(),
            "capture_ocr".to_string(),
            "capture_status".to_string(),
        ],
        ..Default::default()
    }
}

impl ConcurrentHandler for App {
    fn id(&self) -> &str {
        PLUGIN_ID
    }

    fn version(&self) -> &str {
        PLUGIN_VERSION
    }

    fn manifest(&self) -> PluginManifest {
        manifest()
    }

    async fn on_action(&self, req: ActionRequest) -> Vec<Envelope> {
        let params: Value = match serde_json::from_slice(&req.params_json) {
            Ok(v) => v,
            Err(e) => {
                return vec![response_envelope(req.action_id, Err(format!("invalid params_json: {e}")))];
            }
        };

        let result: Result<Value, CaptureError> = match req.action.as_str() {
            "capture_screenshot" => screenshot::capture_screenshot(self.spawner.as_ref(), &params).await,
            "capture_record_start" => record::start(Arc::clone(&self.record_state), Arc::clone(&self.spawner), &params).await,
            "capture_record_stop" => record::stop(&self.record_state, &params).await,
            "capture_ocr" => ocr::capture_ocr(self.spawner.as_ref(), &params).await,
            "capture_status" => {
                let s = session::build_status_report();
                Ok(serde_json::json!({
                    "session_type": s.session_type,
                    "screenshot_backend": s.screenshot_backend,
                    "record_backend": s.record_backend,
                    "ocr_available": s.ocr_available,
                    "portal_available": s.portal_available,
                }))
            }
            other => {
                return vec![response_envelope(req.action_id, Err(format!("unknown action: {other}")))];
            }
        };

        let wire_result = result
            .map(|data| data.to_string().into_bytes())
            .map_err(|e| e.to_string());
        vec![response_envelope(req.action_id, wire_result)]
    }
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let app = Arc::new(App::new());
    let client = VynkorClient::connect_from_env().await?;
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    serve_concurrent(client, &jwt_token, app).await?;
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use vynkor_sdk::concurrent::run_concurrent_loop;
    use vynkor_sdk::proto::{envelope, ActionStatus, PluginShutdown};

    fn action_request(action_id: &str, action: &str, params: Value) -> Envelope {
        Envelope {
            payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                action_id: action_id.to_string(),
                action: action.to_string(),
                params_json: serde_json::to_vec(&params).unwrap(),
                timeout_ms: 0,
                streaming: false,
                caller_plugin_id: "tester".into(),
            })),
            ..Default::default()
        }
    }

    async fn call(
        kernel: &mut VynkorClient,
        action_id: &str,
        action: &str,
        params: Value,
    ) -> Result<Value, String> {
        kernel
            .send("capture", action_request(action_id, action, params))
            .await
            .unwrap();
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for plugin reply")
                .unwrap();
            if let Some(envelope::Payload::ActionResponse(resp)) = env.payload {
                if resp.action_id == action_id {
                    return if resp.status == ActionStatus::ActionOk as i32 {
                        serde_json::from_slice::<Value>(&resp.data_json)
                            .map_err(|e| format!("malformed payload: {e}"))
                    } else {
                        Err(resp.error)
                    };
                }
            }
        }
    }

    async fn shutdown(kernel: &mut VynkorClient, loop_task: tokio::task::JoinHandle<Result<(), VynkorError>>) {
        let shutdown_env = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                reason: "test done".into(),
                grace_seconds: 0,
            })),
            ..Default::default()
        };
        kernel.send("capture", shutdown_env).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("run_concurrent_loop did not exit after PluginShutdown")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn e2e_capture_status_round_trip() {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        let app = Arc::new(App::new());
        let loop_task = tokio::spawn(run_concurrent_loop(client, app));

        let v = call(&mut kernel, "t-1", "capture_status", serde_json::json!({}))
            .await
            .unwrap();
        // `ocr_available` reflects real `tesseract` presence on the machine
        // running the test (see `session::build_status_report`), so this
        // compares against the same live detection rather than hardcoding a
        // value that would be machine-dependent.
        assert_eq!(
            v["ocr_available"].as_bool(),
            Some(session::build_status_report().ocr_available)
        );

        shutdown(&mut kernel, loop_task).await;
    }

    #[tokio::test]
    async fn e2e_unknown_action_is_not_found() {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        let app = Arc::new(App::new());
        let loop_task = tokio::spawn(run_concurrent_loop(client, app));

        let err = call(&mut kernel, "t-2", "capture_bogus", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown action"), "{err}");

        shutdown(&mut kernel, loop_task).await;
    }
}
