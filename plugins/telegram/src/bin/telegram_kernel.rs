//! Telegram plugin — real Grammers integration, runs under vyn kernel.
//! This binary connects to Telegram via MTProto and registers with the kernel.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use telegram_plugin::mtproto::antiban::Antiban;
use telegram_plugin::mtproto::session::SessionPool;
use telegram_plugin::{Config, EventToPublish, HandleResult};
use tokio::sync::{mpsc, oneshot, RwLock};
use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus, Envelope, EventPublish, Pong};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "telegram";
const PLUGIN_VERSION: &str = "0.1.0";

fn manifest() -> vynkor_sdk::proto::PluginManifest {
    vynkor_sdk::proto::PluginManifest {
        permissions: vec![
            "PERMISSION_NETWORK".into(),
            "PERMISSION_STORAGE".into(),
            "PERMISSION_SECRETS".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: vec![
            "status".into(),
            "tg_list_dialogs".into(),
            "tg_get_history".into(),
            "tg_get_message".into(),
            "tg_search".into(),
            "tg_send_message".into(),
        ],
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

/// Shared state for Telegram connections.
struct TelegramState {
    pool: SessionPool,
    antiban: Antiban,
    config: Config,
}

impl TelegramState {
    fn new(config: Config) -> Self {
        Self {
            pool: SessionPool::new(),
            antiban: Antiban::new(),
            config,
        }
    }

    async fn connect_all(&self) {
        for account in &self.config.accounts {
            match self.pool.connect(account).await {
                Ok(()) => println!("[telegram] ✓ {} connected", account.id),
                Err(e) => eprintln!("[telegram] ✗ {} failed: {e}", account.id),
            }
        }
    }
}

/// Handle one action with real Grammers calls.
async fn handle_action_real(
    state: Arc<RwLock<TelegramState>>,
    action: &str,
    params_json: &[u8],
) -> Result<HandleResult, String> {
    let params: Value = serde_json::from_slice(params_json).unwrap_or(Value::Null);
    let state = state.read().await;

    match action {
        "status" => {
            let accounts: Vec<Value> = state
                .config
                .accounts
                .iter()
                .map(|a| {
                    let connected = state.pool.get(&a.id).is_some();
                    serde_json::json!({
                        "id": a.id,
                        "connected": connected,
                    })
                })
                .collect();
            let v = serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "accounts": accounts,
                "default_account": state.config.default_account,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_list_dialogs" => {
            let account_id = resolve_account(&params, &state.config);
            let limit = clamp_limit(&params);

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let mut dialogs = client.iter_dialogs();
            let mut result = Vec::new();
            let mut count = 0u64;

            while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
                if count >= limit {
                    break;
                }
                count += 1;

                let peer_type = match &dialog.chat {
                    grammers_client::types::Chat::User(_) => "user",
                    grammers_client::types::Chat::Group(_) => "group",
                    grammers_client::types::Chat::Channel(_) => "channel",
                };

                let username = dialog.chat.username().unwrap_or("").to_string();

                result.push(serde_json::json!({
                    "id": dialog.chat.id(),
                    "name": dialog.chat.name(),
                    "username": if username.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(username) },
                    "peer_type": peer_type,
                    "last_message": dialog.last_message.as_ref().map(|m| {
                        serde_json::json!({
                            "text": m.text(),
                            "date": m.date().to_rfc3339(),
                            "sender": m.sender().map(|s| s.name().to_string()),
                        })
                    }),
                }));
            }

            let v = serde_json::json!({
                "account": account_id,
                "dialogs": result,
                "total": count,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_get_history" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_get_history: peer required")?;
            let limit = clamp_limit(&params);

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            // Find the dialog by peer id or name
            let mut found_chat = None;
            let mut dialogs = client.iter_dialogs();
            while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
                let id_str = dialog.chat.id().to_string();
                let name = dialog.chat.name().to_string();
                if id_str == peer_id || name.to_lowercase() == peer_id.to_lowercase() {
                    found_chat = Some(dialog.chat.clone());
                    break;
                }
            }

            let chat = found_chat.ok_or_else(|| format!("peer '{peer_id}' not found"))?;
            let mut messages = client.iter_messages(&chat);
            let mut result = Vec::new();
            let mut count = 0u64;

            while let Some(msg) = messages.next().await.map_err(|e| e.to_string())? {
                if count >= limit {
                    break;
                }
                count += 1;

                result.push(serde_json::json!({
                    "id": msg.id(),
                    "text": msg.text(),
                    "date": msg.date().to_rfc3339(),
                    "sender": msg.sender().map(|s| s.name().to_string()),
                    "is_me": msg.sender().map(|s| s.id() == msg.chat().id()).unwrap_or(false),
                }));
            }

            let v = serde_json::json!({
                "peer": peer_id,
                "messages": result,
                "total": count,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_get_message" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_get_message: peer required")?;
            let msg_id = params
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or("tg_get_message: id required")?;

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            // Find the dialog
            let mut found_chat = None;
            let mut dialogs = client.iter_dialogs();
            while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
                let id_str = dialog.chat.id().to_string();
                if id_str == peer_id {
                    found_chat = Some(dialog.chat.clone());
                    break;
                }
            }

            let chat = found_chat.ok_or_else(|| format!("peer '{peer_id}' not found"))?;
            let mut messages = client.iter_messages(&chat);

            while let Some(msg) = messages.next().await.map_err(|e| e.to_string())? {
                if msg.id() as i64 == msg_id {
                    let v = serde_json::json!({
                        "found": true,
                        "message": {
                            "id": msg.id(),
                            "text": msg.text(),
                            "date": msg.date().to_rfc3339(),
                            "sender": msg.sender().map(|s| s.name().to_string()),
                        },
                    });
                    return Ok(HandleResult {
                        data: serde_json::to_vec(&v).unwrap(),
                        event: None,
                    });
                }
            }

            let v = serde_json::json!({"found": false, "message": null});
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_search" => {
            let account_id = resolve_account(&params, &state.config);
            let query = params
                .get("query")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("tg_search: query required")?;
            let limit = clamp_limit(&params);

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            // Search across all dialogs
            let mut result = Vec::new();
            let mut count = 0u64;
            let mut dialogs = client.iter_dialogs();

            while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
                if count >= limit {
                    break;
                }

                let mut messages = client.iter_messages(&dialog.chat);
                while let Some(msg) = messages.next().await.map_err(|e| e.to_string())? {
                    if count >= limit {
                        break;
                    }
                    let text = msg.text().to_string();
                    if text.to_lowercase().contains(&query.to_lowercase()) {
                        count += 1;
                        result.push(serde_json::json!({
                            "id": msg.id(),
                            "text": text,
                            "date": msg.date().to_rfc3339(),
                            "sender": msg.sender().map(|s| s.name().to_string()),
                            "chat": dialog.chat.name(),
                        }));
                    }
                }
            }

            let v = serde_json::json!({
                "query": query,
                "messages": result,
                "total": count,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_send_message" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .unwrap_or("self");
            let text = params
                .get("text")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("tg_send_message: text required")?;

            if text.len() > 4096 {
                return Err("tg_send_message: text too long (max 4096)".into());
            }

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            // Find the peer
            let mut found_chat = None;
            let mut dialogs = client.iter_dialogs();
            while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
                let id_str = dialog.chat.id().to_string();
                let name = dialog.chat.name().to_string();
                if peer_id == "self" || peer_id == "me" {
                    found_chat = Some(dialog.chat.clone());
                    break;
                }
                if id_str == peer_id || name.to_lowercase() == peer_id.to_lowercase() {
                    found_chat = Some(dialog.chat.clone());
                    break;
                }
            }

            let chat = found_chat.ok_or_else(|| format!("peer '{peer_id}' not found"))?;

            let sent = client
                .send_message(&chat, text)
                .await
                .map_err(|e| e.to_string())?;

            let v = serde_json::json!({
                "peer": peer_id,
                "message_id": sent.id(),
                "text": text,
            });
            let ev = EventToPublish {
                event_type: "tg.message_sent".into(),
                payload: serde_json::json!({
                    "peer": peer_id,
                    "message_id": sent.id(),
                }),
            };
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: Some(ev),
            })
        }

        other => Err(format!("unknown action: {other}")),
    }
}

fn resolve_account<'a>(params: &'a Value, config: &'a Config) -> &'a str {
    params
        .get("account")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&config.default_account)
}

fn clamp_limit(params: &Value) -> u64 {
    params
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .clamp(1, 100)
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
    println!("[{PLUGIN_ID}] registered with kernel");

    // Connect to Telegram
    let state = Arc::new(RwLock::new(TelegramState::new(config.clone())));
    state.read().await.connect_all().await;

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(64);
    let mut pending: HashMap<String, oneshot::Sender<Result<Value, String>>> = HashMap::new();

    let st = state.read().await;
    for account in &config.accounts {
        if let Some(client) = st.pool.get(&account.id) {
            let out = outbound_tx.clone();
            let account_id = account.id.clone();
            tokio::spawn(async move {
                println!("[telegram] live listener started for {account_id}");
                loop {
                    match client.next_update().await {
                        Err(e) => {
                            eprintln!("[telegram] {account_id} update error: {e}");
                            break;
                        }
                        Ok(update) => {
                            if let grammers_client::types::Update::NewMessage(msg) = update {
                                let text = msg.text().to_string();
                                let date = msg.date().to_rfc3339();
                                let sender = msg
                                    .sender()
                                    .map(|s| s.name().to_string())
                                    .unwrap_or_default();
                                let chat = msg.chat().name().to_string();
                                let chat_id = msg.chat().id();
                                let msg_id = msg.id();

                                let payload = serde_json::json!({
                                    "account": account_id,
                                    "chat": chat,
                                    "chat_id": chat_id,
                                    "message_id": msg_id,
                                    "sender": sender,
                                    "text": text,
                                    "date": date,
                                });

                                let ev = event_envelope("plugin.telegram.new_message", &payload);
                                let _ = out.send(ev).await;
                            }
                        }
                    }
                }
                println!("[telegram] live listener stopped for {account_id}");
            });
        }
    }
    drop(st);

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
                    Some(envelope::Payload::Event(e)) => {
                        let _ = client.ack_event(&e.event_id).await;
                    }
                    Some(envelope::Payload::EventPublishAck(_)) => {}
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let out = outbound_tx.clone();
                        let state = Arc::clone(&state);
                        let action = req.action.clone();
                        let params = req.params_json.clone();
                        let action_id = req.action_id.clone();

                        tokio::spawn(async move {
                            match handle_action_real(state, &action, &params).await {
                                Ok(res) => {
                                    let _ = out.send(action_response(
                                        action_id,
                                        ActionStatus::ActionOk,
                                        res.data,
                                        String::new(),
                                    )).await;
                                    if let Some(ev) = res.event {
                                        let _ = out.send(event_envelope(&ev.event_type, &ev.payload)).await;
                                    }
                                }
                                Err(e) => {
                                    let _ = out.send(action_response(
                                        action_id,
                                        ActionStatus::ActionError,
                                        Vec::new(),
                                        e,
                                    )).await;
                                }
                            }
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        if let Some(reply) = pending.remove(&resp.action_id) {
                            let result = if resp.status == ActionStatus::ActionOk as i32 {
                                serde_json::from_slice::<Value>(&resp.data_json)
                                    .map_err(|e| format!("malformed payload: {e}"))
                            } else {
                                Err(resp.error)
                            };
                            let _ = reply.send(result);
                        }
                    }
                    _ => {}
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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::from_env();
    let client = VynkorClient::connect_from_env().await?;
    serve(client, config).await
}
