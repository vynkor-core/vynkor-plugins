//! `chats` plugin — host-wide conversation history (sessions, turns, FTS5
//! search, rev-based sync) in one SQLite file. See README.md.
//!
//! All SQLite work happens in `spawn_blocking` under a single
//! `Mutex<Connection>`: there is exactly one writer, which is what makes the
//! per-transaction revision counter strictly monotonic.

use std::sync::{Arc, Mutex};

use chats_plugin::config::Config;
use chats_plugin::{request, schema};
use serde_json::Value;
use tokio::sync::mpsc;
use vynkor_sdk::proto::{
    envelope, ActionResponse, ActionStatus, Envelope, EventPublish, PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "chats";
const PLUGIN_VERSION: &str = "0.1.0";

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_STORAGE".into(), "PERMISSION_EVENT_PUBLISH".into()],
        actions: request::ACTIONS.iter().map(|s| s.to_string()).collect(),
        action_specs: vynkor_plugin_manifest::action_specs(),
        ..Default::default()
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn action_response(action_id: String, status: ActionStatus, data_json: Vec<u8>, error: String) -> Envelope {
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
    std::fs::create_dir_all(&config.data_dir)
        .map_err(|e| VynkorError::Internal(format!("cannot create {}: {e}", config.data_dir.display())))?;
    let conn = schema::open(&config.db_path())
        .map_err(|e| VynkorError::Internal(format!("cannot open chats.db: {e}")))?;
    let conn = Arc::new(Mutex::new(conn));
    let config = Arc::new(config);

    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client.register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token).await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!(
            "registration rejected: {}",
            ack.reject_reason
        )));
    }
    println!("[{PLUGIN_ID}] registered with kernel");

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(64);

    loop {
        tokio::select! {
            env = client.recv() => {
                let env = match env {
                    Ok(e) => e,
                    Err(_) => break,
                };
                match env.payload {
                    Some(envelope::Payload::Ping(ping)) => {
                        let pong = Envelope {
                            payload: Some(envelope::Payload::Pong(Pong {
                                original_timestamp: ping.timestamp,
                                server_timestamp: unix_millis() as u64,
                            })),
                            ..Default::default()
                        };
                        let _ = client.send("kernel", pong).await;
                    }
                    Some(envelope::Payload::PluginShutdown(_)) => break,
                    Some(envelope::Payload::Event(event)) => {
                        let _ = client.ack_event(&event.event_id).await;
                    }
                    Some(envelope::Payload::EventPublishAck(_)) => {}
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let conn = Arc::clone(&conn);
                        let config = Arc::clone(&config);
                        let out = outbound_tx.clone();
                        tokio::spawn(async move {
                            let action = req.action.clone();
                            let params = req.params_json.clone();
                            let res = tokio::task::spawn_blocking(move || {
                                let mut guard = conn.lock().unwrap_or_else(|p| p.into_inner());
                                request::handle(&mut guard, &config, &action, &params, unix_millis())
                            })
                            .await
                            .unwrap_or_else(|e| Err(format!("internal error: {e}")));
                            match res {
                                Ok(val) => {
                                    let data = serde_json::to_vec(&val).unwrap_or_default();
                                    let _ = out.send(action_response(req.action_id, ActionStatus::ActionOk, data, String::new())).await;
                                    if request::is_mutation(&req.action) {
                                        let payload = serde_json::json!({"action": req.action});
                                        let _ = out.send(event_envelope("changed", &payload)).await;
                                    }
                                }
                                Err(e) => {
                                    let _ = out.send(action_response(req.action_id, ActionStatus::ActionError, Vec::new(), e)).await;
                                }
                            }
                        });
                    }
                    other => {
                        println!("[{PLUGIN_ID}] unhandled message: {other:?}");
                    }
                }
            }
            Some(env) = outbound_rx.recv() => {
                let _ = client.send("kernel", env).await;
            }
        }
    }

    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let config = Config::from_env().map_err(VynkorError::Internal)?;
    let socket_path = std::env::var("VYN_SOCKET_PATH")
        .unwrap_or_else(|_| vynkor_wire::socket::default_socket_path());
    let secret = std::env::var("VYN_JWT_SECRET").ok().filter(|s| !s.is_empty());
    let client = match secret {
        Some(s) => VynkorClient::connect_with_secret(&socket_path, s.as_bytes()).await?,
        None => VynkorClient::connect(&socket_path).await?,
    };
    serve(client, config).await
}

#[cfg(test)]
mod manifest_specs_tests {
    use serde_json::Value;

    // Reads the shipped plugin.json directly (see vector-db for why not
    // action_specs()): every declared action needs a description and risk,
    // and the declared list must equal what the dispatcher handles.
    #[test]
    fn manifest_actions_have_docs() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        assert_eq!(specs.len(), parsed["actions"].as_array().unwrap().len());
        let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        let mut handled: Vec<&str> = chats_plugin::request::ACTIONS.to_vec();
        names.sort();
        handled.sort();
        assert_eq!(names, handled, "plugin.json actions must match request::ACTIONS");
        let undocumented: Vec<&str> =
            specs.iter().filter(|s| s.description.is_empty()).map(|s| s.name.as_str()).collect();
        assert!(undocumented.is_empty(), "missing description: {undocumented:?}");
        let unrisked: Vec<&str> = specs
            .iter()
            .filter(|s| s.risk == vynkor_sdk::proto::ActionRisk::Unknown as i32)
            .map(|s| s.name.as_str())
            .collect();
        assert!(unrisked.is_empty(), "missing risk: {unrisked:?}");
    }
}
