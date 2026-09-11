//! telegram plugin — full MTProto user-client (N-account)
//! Single-reader loop owns VynkorClient; workers use Rpc proxy.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use telegram_plugin::{handle_action, Config, Rpc, RpcCall};
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, EventPublish, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "telegram";
const PLUGIN_VERSION: &str = "0.1.0";
const ACTIONS: [&str; 6] = [
    "status",
    "tg_list_dialogs",
    "tg_get_history",
    "tg_get_message",
    "tg_search",
    "tg_send_message",
];

fn manifest() -> vynkor_sdk::proto::PluginManifest {
    vynkor_sdk::proto::PluginManifest {
        permissions: vec![
            "PERMISSION_NETWORK".into(),
            "PERMISSION_STORAGE".into(),
            "PERMISSION_SECRETS".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
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
    let ack = client
        .register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token)
        .await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!(
            "registration rejected: {}",
            ack.reject_reason
        )));
    }
    println!(
        "[{PLUGIN_ID}] registered, accounts={:?}",
        config.accounts.iter().map(|a| &a.id).collect::<Vec<_>>()
    );

    let config = Arc::new(config);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(64);
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCall>(64);
    let rpc = Rpc::new(rpc_tx);

    let mut pending: HashMap<String, (String, oneshot::Sender<Result<Value, String>>)> =
        HashMap::new();
    let mut seq: u64 = 0;

    loop {
        tokio::select! {
            env = client.recv() => {
                let env = match env { Ok(e) => e, Err(_) => break };
                match env.payload {
                    Some(envelope::Payload::Ping(ping)) => {
                        let pong = Envelope { payload: Some(envelope::Payload::Pong(Pong {
                            original_timestamp: ping.timestamp,
                            server_timestamp: unix_millis(),
                        })), ..Default::default() };
                        let _ = client.send("kernel", pong).await;
                    }
                    Some(envelope::Payload::PluginShutdown(_)) => break,
                    Some(envelope::Payload::Event(e)) => { let _ = client.ack_event(&e.event_id).await; }
                    Some(envelope::Payload::EventPublishAck(_)) => {}
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let rpc = rpc.clone();
                        let out = outbound_tx.clone();
                        let cfg = Arc::clone(&config);
                        tokio::spawn(async move {
                            match handle_action(rpc, &cfg, &req.action, &req.params_json).await {
                                Ok(res) => {
                                    let _ = out.send(action_response(req.action_id, ActionStatus::ActionOk, res.data, String::new())).await;
                                    if let Some(ev) = res.event {
                                        let _ = out.send(event_envelope(&ev.event_type, &ev.payload)).await;
                                    }
                                }
                                Err(e) => { let _ = out.send(action_response(req.action_id, ActionStatus::ActionError, Vec::new(), e)).await; }
                            }
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        if let Some((action, reply)) = pending.remove(&resp.action_id) {
                            let result = if resp.status == ActionStatus::ActionOk as i32 {
                                serde_json::from_slice::<Value>(&resp.data_json).map_err(|e| format!("malformed payload: {e}"))
                            } else { Err(format!("{action} failed: {}", resp.error)) };
                            let _ = reply.send(result);
                        }
                    }
                    other => { println!("[{PLUGIN_ID}] unhandled: {other:?}"); }
                }
            }
            Some(env) = outbound_rx.recv() => { let _ = client.send("kernel", env).await; }
            Some(call) = rpc_rx.recv() => {
                seq += 1;
                let action_id = format!("rpc-{seq}");
                pending.insert(action_id.clone(), (call.action.clone(), call.reply));
                let env = Envelope { payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                    action_id, action: call.action, params_json: call.params_json,
                    timeout_ms: call.timeout_ms, streaming: false, ..Default::default()
                })), ..Default::default() };
                let _ = client.send("kernel", env).await;
            }
        }
    }
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let config = Config::from_env();
    let client = VynkorClient::connect_from_env().await?;
    serve(client, config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_returns_accounts() {
        // smoke — handle_action is pure
        let cfg = Config {
            accounts: vec![],
            default_account: "default".into(),
            session_dir: "/tmp".into(),
        };
        let rpc = Rpc::new(mpsc::channel(1).0);
        let res = handle_action(rpc, &cfg, "status", b"{}").await.unwrap();
        let v: Value = serde_json::from_slice(&res.data).unwrap();
        assert_eq!(v["version"], "0.1.0");
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let cfg = Config {
            accounts: vec![],
            default_account: "default".into(),
            session_dir: "/tmp".into(),
        };
        let rpc = Rpc::new(mpsc::channel(1).0);
        let err = handle_action(rpc, &cfg, "bogus", b"{}").await.unwrap_err();
        assert!(err.contains("unknown action"));
    }
}
