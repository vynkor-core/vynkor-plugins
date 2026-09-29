//! `phone` plugin — camera of a phone reached over ssh (or run on the phone
//! itself). See README.md and docs/superpowers/specs/2026-09-29-phone-plugin-design.md.
//!
//! ## Concurrency
//!
//! Uses the SDK's concurrent loop ([`ConcurrentHandler`] + [`serve_concurrent`]),
//! not a sequential one: `phone_photo` blocks for seconds on a network round
//! trip, and a sequential loop would stall the kernel's `Ping` and every other
//! request for that long. The plugin makes no outbound plugin calls, so no
//! RPC proxy is needed.

use std::sync::Arc;

use phone_plugin::config::Config;
use phone_plugin::error::PhoneError;
use phone_plugin::params::{parse_stream_id, PhotoParams, StreamParams};
use phone_plugin::stream::StreamRegistry;
use phone_plugin::transport::{self, Transport};
use phone_plugin::{helper, photo, status};
use serde_json::{json, Value};
use vynkor_sdk::concurrent::{response_envelope, serve_concurrent};
use vynkor_sdk::proto::{ActionRequest, Envelope, PluginManifest};
use vynkor_sdk::{ConcurrentHandler, VynkorClient, VynkorError};

const PLUGIN_ID: &str = "phone";
const PLUGIN_VERSION: &str = "0.1.0";

const ACTIONS: [&str; 6] = [
    "phone_status",
    "phone_setup",
    "phone_photo",
    "phone_stream_start",
    "phone_stream_stop",
    "phone_stream_status",
];

struct App {
    transport: Arc<dyn Transport>,
    cfg: Config,
    reg: StreamRegistry,
}

impl App {
    fn new(cfg: Config, transport: Arc<dyn Transport>) -> Self {
        Self { transport, cfg, reg: StreamRegistry::new() }
    }

    async fn dispatch(&self, action: &str, params: &Value) -> Result<Value, PhoneError> {
        let t = self.transport.as_ref();
        match action {
            "phone_status" => Ok(status::status(t, &self.cfg, &self.reg).await),
            "phone_setup" => {
                let r = helper::setup(t, &self.cfg).await?;
                Ok(json!({
                    "installed": r.installed, "changed": r.changed,
                    "helper_path": r.helper_path, "sha8": r.sha8,
                }))
            }
            "phone_photo" => photo::take(t, &self.cfg, &self.reg, &PhotoParams::parse(params)?).await,
            "phone_stream_start" => self.reg.start(t, &self.cfg, &StreamParams::parse(params)?).await,
            "phone_stream_stop" => self.reg.stop(parse_stream_id(params)?).await,
            "phone_stream_status" => Ok(self.reg.status().await),
            other => Err(PhoneError::BadParams(format!("unknown action: {other}"))),
        }
    }
}

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_NETWORK".to_string(), "PERMISSION_SCREEN".to_string()],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
        action_specs: vynkor_plugin_manifest::action_specs(),
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
        let params: Value = if req.params_json.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice(&req.params_json) {
                Ok(v) => v,
                Err(e) => {
                    return vec![response_envelope(req.action_id, Err(format!("invalid params_json: {e}")))];
                }
            }
        };
        let wire = self
            .dispatch(&req.action, &params)
            .await
            .map(|v| v.to_string().into_bytes())
            .map_err(|e| e.to_string());
        vec![response_envelope(req.action_id, wire)]
    }
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[{PLUGIN_ID}] bad configuration: {e}");
            std::process::exit(2);
        }
    };
    let transport = transport::from_config(&cfg);
    eprintln!("[{PLUGIN_ID}] transport {}", transport.describe());
    let app = Arc::new(App::new(cfg, transport));
    let client = VynkorClient::connect_from_env().await?;
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    serve_concurrent(client, &jwt_token, app).await?;
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phone_plugin::framing::encode_frame;
    use phone_plugin::transport::fake::FakeTransport;
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

    async fn call(kernel: &mut VynkorClient, id: &str, action: &str, params: Value) -> Result<Value, String> {
        kernel.send("phone", action_request(id, action, params)).await.unwrap();
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for plugin reply")
                .unwrap();
            if let Some(envelope::Payload::ActionResponse(resp)) = env.payload {
                if resp.action_id == id {
                    return if resp.status == ActionStatus::ActionOk as i32 {
                        serde_json::from_slice::<Value>(&resp.data_json).map_err(|e| format!("malformed payload: {e}"))
                    } else {
                        Err(resp.error)
                    };
                }
            }
        }
    }

    fn test_cfg(dir: &std::path::Path) -> Config {
        Config::from_lookup(|k| match k {
            "PHONE_PLUGIN_DIR" => Some(dir.to_string_lossy().into_owned()),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn shipped_manifest_matches_the_served_actions_and_documents_each() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        let mut declared: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        declared.sort();
        let mut served: Vec<String> = ACTIONS.iter().map(|s| s.to_string()).collect();
        served.sort();
        assert_eq!(declared, served, "plugin.json and ACTIONS must list exactly the same actions");
        let undocumented: Vec<&str> = specs.iter().filter(|s| s.description.is_empty()).map(|s| s.name.as_str()).collect();
        assert!(undocumented.is_empty(), "actions missing a description: {undocumented:?}");
        for s in &specs {
            assert!(s.risk != 0, "{} must declare a risk", s.name);
        }
        let high: Vec<&str> = specs.iter().filter(|s| s.requires_confirmation).map(|s| s.name.as_str()).collect();
        assert!(high.contains(&"phone_photo") && high.contains(&"phone_stream_start"), "{high:?}");
    }

    #[test]
    fn manifest_declares_the_two_permissions() {
        assert_eq!(manifest().permissions, vec!["PERMISSION_NETWORK", "PERMISSION_SCREEN"]);
    }

    #[tokio::test]
    async fn e2e_status_setup_photo_and_error_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTransport::new());
        let jpeg = vec![0xFF, 0xD8, 7, 7, 0xFF, 0xD9];
        fake.push_run(FakeTransport::ok(Vec::new())); // status: helper missing
        fake.push_run(FakeTransport::ok(Vec::new())); // setup: check -> missing
        fake.push_run(FakeTransport::ok(Vec::new())); // setup: deploy
        fake.push_run(FakeTransport::ok(encode_frame(&jpeg))); // photo

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        let app = Arc::new(App::new(test_cfg(dir.path()), fake.clone() as Arc<dyn Transport>));
        let loop_task = tokio::spawn(run_concurrent_loop(client, app));

        let v = call(&mut kernel, "t-1", "phone_status", json!({})).await.unwrap();
        assert_eq!(v["helper"], json!("missing"));
        assert_eq!(v["reachable"], json!(true));

        let v = call(&mut kernel, "t-2", "phone_setup", json!({})).await.unwrap();
        assert_eq!(v["changed"], json!(true));

        let v = call(&mut kernel, "t-3", "phone_photo", json!({"camera": "front", "width": 640, "height": 480})).await.unwrap();
        assert_eq!(std::fs::read(v["path"].as_str().unwrap()).unwrap(), jpeg);
        assert_eq!(v["camera"], json!("front"));

        // bad params and unknown actions come back as coded errors, not crashes
        let e = call(&mut kernel, "t-4", "phone_photo", json!({"camera": "side"})).await.unwrap_err();
        assert!(e.starts_with("ERR_PHONE_BAD_PARAMS"), "{e}");
        let e = call(&mut kernel, "t-5", "phone_nope", json!({})).await.unwrap_err();
        assert!(e.contains("unknown action"), "{e}");

        let v = call(&mut kernel, "t-6", "phone_stream_status", json!({})).await.unwrap();
        assert_eq!(v, json!({"active": false}));
        let v = call(&mut kernel, "t-7", "phone_stream_stop", json!({})).await.unwrap();
        assert_eq!(v["stopped"], json!(false));

        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown { reason: "test done".into(), grace_seconds: 0 })),
            ..Default::default()
        };
        kernel.send("phone", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task).await.expect("loop did not exit").unwrap().unwrap();
    }
}
