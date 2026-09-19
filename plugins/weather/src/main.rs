//! `weather` plugin — open-meteo current + forecast via `network`'s http_request.
//! No API key, fully outbound, `PERMISSION_NETWORK` caller gate (T-19).

use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus, Envelope, PluginManifest, Pong};
use vynkor_sdk::{VynkorClient, VynkorError};
use weather_plugin::handler;

const PLUGIN_ID: &str = "weather";
const PLUGIN_VERSION: &str = "0.1.0";

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_NETWORK".into()],
        actions: vec!["weather_now".into(), "weather_forecast".into(), "status".into()],
        action_specs: vynkor_plugin_manifest::action_specs(),
        ..Default::default()
    }
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
fn start_instant() -> std::time::Instant { *START.get_or_init(std::time::Instant::now) }
fn status_payload() -> Vec<u8> {
    let uptime_ms = start_instant().elapsed().as_millis() as u64;
    serde_json::to_vec(&serde_json::json!({
        "version": PLUGIN_VERSION,
        "uptime_ms": uptime_ms,
        "engine_ready": true,
        "last_error": null,
        "counters": {}
    })).unwrap()
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn handle_action_request(
    client: &mut VynkorClient,
    req: vynkor_sdk::proto::ActionRequest,
) -> Envelope {
    let reply = match req.action.as_str() {
        "weather_now" => match handler::handle_weather_now(client, &req.params_json).await {
            Ok(data_json) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionOk as i32,
                data_json,
                error: String::new(),
            },
            Err(e) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionError as i32,
                data_json: Vec::new(),
                error: e,
            },
        },
        "weather_forecast" => match handler::handle_weather_forecast(client, &req.params_json).await {
            Ok(data_json) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionOk as i32,
                data_json,
                error: String::new(),
            },
            Err(e) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionError as i32,
                data_json: Vec::new(),
                error: e,
            },
        },
        "status" => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionOk as i32,
            data_json: status_payload(),
            error: String::new(),
        },
        other => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionNotFound as i32,
            data_json: Vec::new(),
            error: format!("unknown action: {other}"),
        },
    };
    Envelope {
        payload: Some(envelope::Payload::ActionResponse(reply)),
        ..Default::default()
    }
}

async fn serve(mut client: VynkorClient) -> Result<(), VynkorError> {
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
            Ok(e) => e,
            Err(_) => break,
        };
        match env.payload {
            Some(envelope::Payload::Ping(p)) => {
                let pong = Envelope {
                    payload: Some(envelope::Payload::Pong(Pong {
                        original_timestamp: p.timestamp,
                        server_timestamp: unix_millis(),
                    })),
                    ..Default::default()
                };
                let _ = client.send("kernel", pong).await;
            }
            Some(envelope::Payload::PluginShutdown(_)) => break,
            Some(envelope::Payload::Event(e)) => {
                let _ = client.ack_event(&e.event_id).await;
            }
            Some(envelope::Payload::ActionRequest(req)) => {
                let resp = handle_action_request(&mut client, req).await;
                let _ = client.send("kernel", resp).await;
            }
            other => println!("[{PLUGIN_ID}] unhandled: {other:?}"),
        }
    }
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let socket_path = std::env::var("VYN_SOCKET_PATH")
        .unwrap_or_else(|_| vynkor_wire::socket::default_socket_path());
    let secret = std::env::var("VYN_JWT_SECRET").ok().filter(|s| !s.is_empty());
    let client = match secret {
        Some(s) => VynkorClient::connect_with_secret(&socket_path, s.as_bytes()).await?,
        None => VynkorClient::connect(&socket_path).await?,
    };
    serve(client).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;
    use vynkor_sdk::proto::{ActionResponse as ProtoActionResponse, PluginRegisterAck};

    const CURRENT_FIXTURE: &str = r#"{"latitude":52.5,"longitude":13.4,"timezone":"auto","current":{"temperature_2m":21.3,"wind_speed_10m":5.1}}"#;
    const DAILY_FIXTURE: &str = r#"{"latitude":52.5,"longitude":13.4,"timezone":"auto","daily":{"time":["2026-08-26"],"temperature_2m_max":[22.0]}}"#;

    type HttpRequests = Arc<Mutex<Vec<serde_json::Value>>>;

    enum Cmd {
        Call { action: String, params: serde_json::Value, reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>> },
    }

    struct Shim { tx: tokio::sync::mpsc::Sender<Cmd>, http: HttpRequests }

    impl Shim {
        async fn call(&self, action: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.tx.send(Cmd::Call { action: action.into(), params, reply: tx }).await.expect("shim died");
            tokio::time::timeout(Duration::from_secs(5), rx).await.expect("timeout").expect("dropped")
        }
        async fn http_requests(&self) -> Vec<serde_json::Value> { self.http.lock().await.clone() }
    }

    async fn start_with(body: &str) -> Shim {
        let http_data = serde_json::json!({"status": 200, "body": body, "body_encoding": "utf8"});
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let pc = VynkorClient::from_stream(plugin_side, None);
        let kc = VynkorClient::from_stream(kernel_side, None);
        tokio::spawn(async move { let _ = serve(pc).await; });
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let http: HttpRequests = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_shim(kc, rx, http.clone(), http_data));
        Shim { tx, http }
    }

    async fn run_shim(mut kernel: VynkorClient, mut rx: tokio::sync::mpsc::Receiver<Cmd>, http: HttpRequests, http_data: serde_json::Value) {
        let mut pending: StdHashMap<String, tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>> = StdHashMap::new();
        let mut seq = 0u64;
        // handshake
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv()).await.expect("timeout").expect("closed");
            if let Some(envelope::Payload::PluginRegister(_)) = env.payload {
                let _ = kernel.send("weather", Envelope { payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck { accepted: true, ..Default::default() })), ..Default::default() }).await;
                break;
            }
        }
        loop {
            tokio::select! {
                env = kernel.recv() => {
                    let env = match env { Ok(e) => e, Err(_) => break };
                    match env.payload {
                        Some(envelope::Payload::ActionRequest(req)) => {
                            if req.action == "http_request" {
                                let p: serde_json::Value = serde_json::from_slice(&req.params_json).unwrap_or_default();
                                http.lock().await.push(p);
                                let resp = ProtoActionResponse { action_id: req.action_id, status: ActionStatus::ActionOk as i32, data_json: serde_json::to_vec(&http_data).unwrap(), error: String::new() };
                                let _ = kernel.send("weather", Envelope { payload: Some(envelope::Payload::ActionResponse(resp)), ..Default::default() }).await;
                            }
                        }
                        Some(envelope::Payload::ActionResponse(r)) => {
                            if let Some(tx) = pending.remove(&r.action_id) {
                                let res = if r.status == ActionStatus::ActionOk as i32 { serde_json::from_slice(&r.data_json).map_err(|e| e.to_string()) } else { Err(r.error) };
                                let _ = tx.send(res);
                            }
                        }
                        Some(envelope::Payload::Ping(p)) => { let _ = kernel.send("weather", Envelope { payload: Some(envelope::Payload::Pong(Pong { original_timestamp: p.timestamp, server_timestamp: unix_millis() })), ..Default::default() }).await; }
                        _ => {}
                    }
                }
                cmd = rx.recv() => {
                    match cmd { Some(Cmd::Call { action, params, reply }) => { seq+=1; let id=format!("t-{seq}"); pending.insert(id.clone(), reply); let env=Envelope { payload: Some(envelope::Payload::ActionRequest(vynkor_sdk::proto::ActionRequest { action_id: id, action, params_json: serde_json::to_vec(&params).unwrap(), timeout_ms: 0, streaming: false, ..Default::default() })), ..Default::default() }; let _ = kernel.send("weather", env).await; } None => break }
                }
            }
        }
    }

    #[tokio::test]
    async fn weather_now_end_to_end() {
        let shim = start_with(CURRENT_FIXTURE).await;
        let out = shim.call("weather_now", serde_json::json!({"lat": 52.5, "lon": 13.4})).await.unwrap();
        assert_eq!(out["current"]["temperature_2m"], 21.3);
        let reqs = shim.http_requests().await;
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0]["url"].as_str().unwrap().contains("api.open-meteo.com"));
    }

    #[tokio::test]
    async fn weather_forecast_end_to_end() {
        let shim = start_with(DAILY_FIXTURE).await;
        let out = shim.call("weather_forecast", serde_json::json!({"lat": 52.5, "lon": 13.4, "days": 1})).await.unwrap();
        assert!(out["daily"]["temperature_2m_max"][0].as_f64().unwrap() == 22.0);
    }

    #[tokio::test]
    async fn rejects_oob_lat() {
        let shim = start_with(CURRENT_FIXTURE).await;
        let err = shim.call("weather_now", serde_json::json!({"lat": 100, "lon": 0})).await.unwrap_err();
        assert!(err.contains("lat"), "{err}");
    }

    #[tokio::test]
    async fn unknown_action() {
        let shim = start_with(CURRENT_FIXTURE).await;
        let err = shim.call("weather_unknown", serde_json::json!({"lat": 0, "lon": 0})).await.unwrap_err();
        assert!(err.contains("unknown"), "{err}");
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
