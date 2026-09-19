use std::collections::HashMap;
use std::sync::Arc;
use github_plugin::{handle_action, Config, Rpc, RpcCall};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, Pong, PluginManifest};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "github";
const PLUGIN_VERSION: &str = "0.1.0";
const ACTIONS: [&str; 4] = ["gh_list_issues", "gh_create_issue", "gh_list_prs", "gh_list_runs"];

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_NETWORK".into(), "PERMISSION_SECRETS".into()],
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

async fn serve(mut client: VynkorClient, config: Config) -> Result<(), VynkorError> {
    let jwt = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client.register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt).await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!("rejected: {}", ack.reject_reason)));
    }
    println!("[{PLUGIN_ID}] registered");
    let start = std::time::Instant::now();
    let config = Arc::new(config);
    let (out_tx, mut out_rx) = mpsc::channel::<Envelope>(64);
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCall>(64);
    let rpc = Rpc::new(rpc_tx);
    let mut pending: HashMap<String, (String, oneshot::Sender<Result<Value, String>>)> = HashMap::new();
    let mut seq: u64 = 0;
    loop {
        tokio::select! {
            env = client.recv() => {
                let env = match env { Ok(e) => e, Err(_) => break };
                match env.payload {
                    Some(envelope::Payload::Ping(p)) => {
                        let pong = Envelope { payload: Some(envelope::Payload::Pong(Pong { original_timestamp: p.timestamp, server_timestamp: unix_millis() })), ..Default::default() };
                        let _ = client.send("kernel", pong).await;
                    }
                    Some(envelope::Payload::PluginShutdown(_)) => break,
                    Some(envelope::Payload::Event(e)) => { let _ = client.ack_event(&e.event_id).await; }
                    Some(envelope::Payload::EventPublishAck(_)) => {}
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let rpc = rpc.clone();
                        let out = out_tx.clone();
                        let cfg = Arc::clone(&config);
                        tokio::spawn(async move {
                            match handle_action(rpc, &cfg, &req.action, &req.params_json, start).await {
                                Ok(res) => {
                                    let _ = out.send(action_response(req.action_id, ActionStatus::ActionOk, res.data, String::new())).await;
                                    if let Some((t, p)) = res.event {
                                        let _ = out.send(Envelope { payload: Some(envelope::Payload::EventPublish(vynkor_sdk::proto::EventPublish { event_type: t, payload_json: p.to_string().into_bytes() })), ..Default::default() }).await;
                                    }
                                }
                                Err(e) => { let _ = out.send(action_response(req.action_id, ActionStatus::ActionError, Vec::new(), e)).await; }
                            }
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        if let Some((action, reply)) = pending.remove(&resp.action_id) {
                            let result = if resp.status == ActionStatus::ActionOk as i32 {
                                serde_json::from_slice::<Value>(&resp.data_json).map_err(|e| format!("malformed: {e}"))
                            } else { Err(format!("{action} failed: {}", resp.error)) };
                            let _ = reply.send(result);
                        }
                    }
                    other => println!("[{PLUGIN_ID}] unhandled {other:?}"),
                }
            }
            Some(env) = out_rx.recv() => { let _ = client.send("kernel", env).await; }
            Some(call) = rpc_rx.recv() => {
                seq += 1;
                let action_id = format!("rpc-{seq}");
                pending.insert(action_id.clone(), (call.action.clone(), call.reply));
                let env = Envelope { payload: Some(envelope::Payload::ActionRequest(ActionRequest { action_id, action: call.action, params_json: call.params_json, timeout_ms: call.timeout_ms, streaming: false, ..Default::default() })), ..Default::default() };
                let _ = client.send("kernel", env).await;
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
