use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vector_db_plugin::config::Config;
use vector_db_plugin::db::DbPools;
use vector_db_plugin::handler::{Handler, Rpc, RpcCall};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, EventPublish, PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "vector-db";
const PLUGIN_VERSION: &str = "0.1.0";

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec![
            "PERMISSION_STORAGE".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: vec![
            "vec_upsert".into(),
            "vec_upsert_batch".into(),
            "vec_query".into(),
            "vec_get".into(),
            "vec_delete".into(),
            "vec_list".into(),
            "vec_stats".into(),
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
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client.register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token).await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!(
            "registration rejected: {}",
            ack.reject_reason
        )));
    }
    println!("[{PLUGIN_ID}] registered with kernel");

    let pools = DbPools::new(config.db.clone());
    let handler = Arc::new(Handler::new(
        pools,
        config.max_response_bytes,
        config.default_dim,
    ));
    let embed_cfg = Arc::new(config.embed.clone());

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(64);
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCall>(64);
    let rpc = Rpc::new(rpc_tx);

    let mut pending: HashMap<String, (String, oneshot::Sender<Result<Value, String>>)> = HashMap::new();
    let mut seq: u64 = 0;

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
                    Some(envelope::Payload::EventPublishAck(_)) => {}
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let handler = Arc::clone(&handler);
                        let embed_cfg = Arc::clone(&embed_cfg);
                        let rpc = rpc.clone();
                        let out = outbound_tx.clone();
                        let caller = req.caller_plugin_id.clone();
                        let action = req.action.clone();
                        let params = req.params_json.clone();
                        let action_id = req.action_id.clone();
                        tokio::spawn(async move {
                            let rpc_opt = if embed_cfg.enabled { Some(rpc) } else { None };
                            let res = handler
                                .handle_with_rpc(&caller, &action, &params, rpc_opt, Some(&embed_cfg))
                                .await;
                            match res {
                                Ok(val) => {
                                    let data = serde_json::to_vec(&val).unwrap_or_default();
                                    let _ = out.send(action_response(action_id.clone(), ActionStatus::ActionOk, data, String::new())).await;
                                    if action == "vec_upsert" || action == "vec_upsert_batch" || action == "vec_delete" {
                                        let payload = serde_json::json!({"caller": caller, "action": action});
                                        let _ = out.send(event_envelope("changed", &payload)).await;
                                    }
                                }
                                Err(e) => {
                                    let _ = out.send(action_response(action_id, ActionStatus::ActionError, Vec::new(), e)).await;
                                }
                            }
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        if let Some((action, reply)) = pending.remove(&resp.action_id) {
                            let result = if resp.status == ActionStatus::ActionOk as i32 {
                                serde_json::from_slice::<Value>(&resp.data_json)
                                    .map_err(|e| format!("malformed ai response: {e}"))
                            } else {
                                Err(format!("ai.{} failed: {}", action, resp.error))
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
    let config = Config::from_env();
    serve(client, config).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use vector_db_plugin::db::{DbConfig, DbPools};
    use vector_db_plugin::handler::Handler;
    use vynkor_sdk::concurrent::run_concurrent_loop;
    use vynkor_sdk::proto::{envelope, ActionRequest, ActionStatus, Envelope, PluginShutdown};
    use vynkor_sdk::VynkorClient;

    #[tokio::test]
    async fn concurrent_upserts_without_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let pools = DbPools::new(DbConfig {
            data_dir: dir.path().to_path_buf(),
            pool_size: 4,
            busy_timeout_ms: 2000,
            max_db_bytes: 0,
        });
        let handler = Arc::new(Handler::new(pools, 4 * 1024 * 1024, 8));

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);

        let loop_task = tokio::spawn(run_concurrent_loop(client, handler));

        const N: usize = 20;
        for i in 0..N {
            let req = Envelope {
                payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                    action_id: format!("action-{i}"),
                    action: "vec_upsert".into(),
                    params_json: serde_json::to_vec(&serde_json::json!({
                        "collection": "test",
                        "id": format!("doc:{i}"),
                        "text": format!("hello world {i}")
                    }))
                    .unwrap(),
                    timeout_ms: 0,
                    streaming: false,
                    caller_plugin_id: "caller_x".into(),
                })),
                ..Default::default()
            };
            kernel.send("vector-db", req).await.unwrap();
        }

        let mut seen = 0;
        let mut changed_events = 0usize;
        for _ in 0..(2 * N) {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out — loop likely deadlocked")
                .unwrap();
            match env.payload {
                Some(envelope::Payload::ActionResponse(resp)) => {
                    assert_eq!(resp.status, ActionStatus::ActionOk as i32, "error: {}", resp.error);
                    seen += 1;
                }
                Some(envelope::Payload::EventPublish(ev)) => {
                    assert_eq!(ev.event_type, "changed");
                    changed_events += 1;
                }
                other => panic!("unexpected payload: {other:?}"),
            }
        }
        assert_eq!(seen, N);
        assert_eq!(changed_events, N);

        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                reason: "test done".into(),
                grace_seconds: 0,
            })),
            ..Default::default()
        };
        kernel.send("vector-db", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
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
