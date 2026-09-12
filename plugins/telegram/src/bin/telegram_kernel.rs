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
            "tg_get_dialog_info".into(),
            "tg_get_history".into(),
            "tg_get_message".into(),
            "tg_search".into(),
            "tg_send_message".into(),
            "tg_send_photo".into(),
            "tg_send_document".into(),
            "tg_edit_message".into(),
            "tg_delete_message".into(),
            "tg_forward_message".into(),
            "tg_pin_message".into(),
            "tg_mark_read".into(),
            "tg_download_media".into(),
            "tg_react".into(),
            "tg_get_message_link".into(),
            "tg_get_chat_link".into(),
            "tg_export_history".into(),
            "tg_get_participants".into(),
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
            let offset_id = params
                .get("offset_id")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32);

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;
            let mut messages = client.iter_messages(&chat);
            if let Some(offset) = offset_id {
                messages = messages.offset_id(offset);
            }
            let mut result = Vec::new();
            let mut count = 0u64;

            while let Some(msg) = messages.next().await.map_err(|e| e.to_string())? {
                if count >= limit {
                    break;
                }
                count += 1;

                let sender_id = msg.sender().map(|s| s.id());
                let chat_id = msg.chat().id();
                let is_me = sender_id.map(|id| id == chat_id).unwrap_or(false);

                result.push(serde_json::json!({
                    "id": msg.id(),
                    "text": msg.text(),
                    "date": msg.date().to_rfc3339(),
                    "sender": msg.sender().map(|s| s.name().to_string()),
                    "sender_id": sender_id,
                    "is_me": is_me,
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

            let chat = find_chat(&client, peer_id).await?;
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
            let peer_id = params.get("peer").and_then(|v| v.as_str());

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let mut result = Vec::new();
            let mut count = 0u64;

            if let Some(peer) = peer_id {
                let mut found_chat = None;
                let search = peer.strip_prefix('@').unwrap_or(peer);
                let mut dialogs = client.iter_dialogs();
                while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
                    let id_str = dialog.chat.id().to_string();
                    let name = dialog.chat.name().to_string();
                    let username = dialog.chat.username().unwrap_or("").to_string();
                    if id_str == peer
                        || name.to_lowercase() == search.to_lowercase()
                        || (!username.is_empty()
                            && username.to_lowercase() == search.to_lowercase())
                    {
                        found_chat = Some(dialog.chat.clone());
                        break;
                    }
                }
                if let Some(chat) = found_chat {
                    let mut search_iter = client.search_messages(&chat).query(query);
                    while let Some(msg) = search_iter.next().await.map_err(|e| e.to_string())? {
                        if count >= limit {
                            break;
                        }
                        count += 1;
                        result.push(serde_json::json!({
                            "id": msg.id(),
                            "text": msg.text(),
                            "date": msg.date().to_rfc3339(),
                            "sender": msg.sender().map(|s| s.name().to_string()),
                            "chat": chat.name(),
                        }));
                    }
                }
            } else {
                let mut search_iter = client.search_all_messages().query(query);
                while let Some(msg) = search_iter.next().await.map_err(|e| e.to_string())? {
                    if count >= limit {
                        break;
                    }
                    count += 1;
                    result.push(serde_json::json!({
                        "id": msg.id(),
                        "text": msg.text(),
                        "date": msg.date().to_rfc3339(),
                        "sender": msg.sender().map(|s| s.name().to_string()),
                        "chat": msg.chat().name(),
                    }));
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
            let reply_to = params.get("reply_to").and_then(|v| v.as_i64());

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

            let chat = find_chat(&client, peer_id).await?;

            let mut msg = grammers_client::types::InputMessage::text(text);
            if let Some(reply_id) = reply_to {
                msg = msg.reply_to(Some(reply_id as i32));
            }
            let sent = client
                .send_message(&chat, msg)
                .await
                .map_err(|e| e.to_string())?;

            let v = serde_json::json!({
                "peer": peer_id,
                "message_id": sent.id(),
                "text": text,
                "reply_to": reply_to,
            });
            let ev = EventToPublish {
                event_type: "tg.message_sent".into(),
                payload: serde_json::json!({
                    "peer": peer_id,
                    "message_id": sent.id(),
                    "reply_to": reply_to,
                }),
            };
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: Some(ev),
            })
        }

        "tg_get_dialog_info" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_get_dialog_info: peer required")?;

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;

            let peer_type = match &chat {
                grammers_client::types::Chat::User(_) => "user",
                grammers_client::types::Chat::Group(_) => "group",
                grammers_client::types::Chat::Channel(_) => "channel",
            };

            let username = chat.username().unwrap_or("").to_string();

            let mut info = serde_json::json!({
                "id": chat.id(),
                "name": chat.name(),
                "peer_type": peer_type,
            });

            if !username.is_empty() {
                info["username"] = serde_json::Value::String(username);
            }

            match &chat {
                grammers_client::types::Chat::User(user) => {
                    info["first_name"] = serde_json::Value::String(user.first_name().to_string());
                    if let Some(last) = user.last_name() {
                        info["last_name"] = serde_json::Value::String(last.to_string());
                    }
                    info["bot"] = serde_json::Value::Bool(user.is_bot());
                }
                grammers_client::types::Chat::Group(group) => {
                    info["title"] = serde_json::Value::String(group.title().to_string());
                }
                grammers_client::types::Chat::Channel(channel) => {
                    info["title"] = serde_json::Value::String(channel.title().to_string());
                }
            }

            Ok(HandleResult {
                data: serde_json::to_vec(&info).unwrap(),
                event: None,
            })
        }

        "tg_edit_message" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_edit_message: peer required")?;
            let msg_id = params
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or("tg_edit_message: id required")?;
            let text = params
                .get("text")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("tg_edit_message: text required")?;

            if text.len() > 4096 {
                return Err("tg_edit_message: text too long (max 4096)".into());
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

            let chat = find_chat(&client, peer_id).await?;
            client
                .edit_message(&chat, msg_id as i32, text)
                .await
                .map_err(|e| e.to_string())?;

            let v = serde_json::json!({
                "peer": peer_id,
                "message_id": msg_id,
                "edited": true,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_delete_message" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_delete_message: peer required")?;
            let msg_ids: Vec<i32> = params
                .get("ids")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_i64().map(|id| id as i32))
                        .collect()
                })
                .unwrap_or_default();

            if msg_ids.is_empty() {
                return Err("tg_delete_message: ids required".into());
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

            let chat = find_chat(&client, peer_id).await?;
            let deleted = client
                .delete_messages(&chat, &msg_ids)
                .await
                .map_err(|e| e.to_string())?;

            let v = serde_json::json!({
                "peer": peer_id,
                "deleted_count": deleted,
                "requested": msg_ids.len(),
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_forward_message" => {
            let account_id = resolve_account(&params, &state.config);
            let from_peer = params
                .get("from")
                .and_then(|v| v.as_str())
                .ok_or("tg_forward_message: from required")?;
            let to_peer = params
                .get("to")
                .and_then(|v| v.as_str())
                .ok_or("tg_forward_message: to required")?;
            let msg_ids: Vec<i32> = params
                .get("ids")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_i64().map(|id| id as i32))
                        .collect()
                })
                .unwrap_or_default();

            if msg_ids.is_empty() {
                return Err("tg_forward_message: ids required".into());
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

            let from_chat = find_chat(&client, from_peer).await?;
            let to_chat = find_chat(&client, to_peer).await?;

            let forwarded = client
                .forward_messages(&to_chat, &msg_ids, &from_chat)
                .await
                .map_err(|e| e.to_string())?;

            let ids: Vec<i32> = forwarded
                .iter()
                .filter_map(|m| m.as_ref().map(|msg| msg.id()))
                .collect();
            let v = serde_json::json!({
                "from": from_peer,
                "to": to_peer,
                "forwarded_ids": ids,
                "count": ids.len(),
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_get_participants" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_get_participants: peer required")?;
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

            let chat = find_chat(&client, peer_id).await?;
            let mut participants = client.iter_participants(&chat);
            let mut result = Vec::new();
            let mut count = 0u64;

            while let Some(participant) = participants.next().await.map_err(|e| e.to_string())? {
                if count >= limit {
                    break;
                }
                count += 1;

                let user = &participant.user;
                result.push(serde_json::json!({
                    "id": user.id(),
                    "name": user.full_name(),
                    "username": user.username(),
                    "bot": user.is_bot(),
                }));
            }

            let v = serde_json::json!({
                "peer": peer_id,
                "participants": result,
                "total": count,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_mark_read" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_mark_read: peer required")?;

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;
            client
                .mark_as_read(&chat)
                .await
                .map_err(|e| e.to_string())?;

            let v = serde_json::json!({
                "peer": peer_id,
                "marked_as_read": true,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_get_unread" => {
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

                result.push(serde_json::json!({
                    "id": dialog.chat.id(),
                    "name": dialog.chat.name(),
                    "username": dialog.chat.username().unwrap_or(""),
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

        "tg_send_photo" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .unwrap_or("self");
            let file_path = params
                .get("file")
                .and_then(|v| v.as_str())
                .ok_or("tg_send_photo: file required")?;
            let caption = params.get("caption").and_then(|v| v.as_str()).unwrap_or("");

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;

            let uploaded = client
                .upload_file(file_path)
                .await
                .map_err(|e| format!("failed to upload file: {e}"))?;

            let msg = grammers_client::types::InputMessage::text(caption).photo(uploaded);
            let sent = client
                .send_message(&chat, msg)
                .await
                .map_err(|e| e.to_string())?;

            let v = serde_json::json!({
                "peer": peer_id,
                "message_id": sent.id(),
                "file": file_path,
                "caption": caption,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_send_document" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .unwrap_or("self");
            let file_path = params
                .get("file")
                .and_then(|v| v.as_str())
                .ok_or("tg_send_document: file required")?;
            let caption = params.get("caption").and_then(|v| v.as_str()).unwrap_or("");

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;

            let uploaded = client
                .upload_file(file_path)
                .await
                .map_err(|e| format!("failed to upload file: {e}"))?;

            let msg = grammers_client::types::InputMessage::text(caption).document(uploaded);
            let sent = client
                .send_message(&chat, msg)
                .await
                .map_err(|e| e.to_string())?;

            let v = serde_json::json!({
                "peer": peer_id,
                "message_id": sent.id(),
                "file": file_path,
                "caption": caption,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_pin_message" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_pin_message: peer required")?;
            let msg_id = params
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or("tg_pin_message: id required")?;

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;
            client
                .pin_message(&chat, msg_id as i32)
                .await
                .map_err(|e| e.to_string())?;

            let v = serde_json::json!({
                "peer": peer_id,
                "message_id": msg_id,
                "pinned": true,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_download_media" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_download_media: peer required")?;
            let msg_id = params
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or("tg_download_media: id required")?;
            let output_path = params
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("tg_download_media: path required")?;

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;
            let mut messages = client.iter_messages(&chat);

            while let Some(msg) = messages.next().await.map_err(|e| e.to_string())? {
                if msg.id() as i64 == msg_id {
                    if msg.media().is_none() {
                        return Err("tg_download_media: no media in message".into());
                    }
                    msg.download_media(output_path)
                        .await
                        .map_err(|e| format!("download failed: {e}"))?;
                    let v = serde_json::json!({
                        "peer": peer_id,
                        "message_id": msg_id,
                        "path": output_path,
                        "downloaded": true,
                    });
                    return Ok(HandleResult {
                        data: serde_json::to_vec(&v).unwrap(),
                        event: None,
                    });
                }
            }
            Err(format!("message {msg_id} not found in {peer_id}"))
        }

        "tg_react" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_react: peer required")?;
            let msg_id = params
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or("tg_react: id required")?;
            let emoji = params
                .get("emoji")
                .and_then(|v| v.as_str())
                .ok_or("tg_react: emoji required")?;

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;
            let reactions = grammers_client::types::InputReactions::emoticon(emoji);
            client
                .send_reactions(&chat, msg_id as i32, reactions)
                .await
                .map_err(|e| e.to_string())?;

            let v = serde_json::json!({
                "peer": peer_id,
                "message_id": msg_id,
                "emoji": emoji,
                "reacted": true,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_get_message_link" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_get_message_link: peer required")?;
            let msg_id = params
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or("tg_get_message_link: id required")?;

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;
            let mut messages = client.iter_messages(&chat);

            while let Some(msg) = messages.next().await.map_err(|e| e.to_string())? {
                if msg.id() as i64 == msg_id {
                    let link = match &chat {
                        grammers_client::types::Chat::Channel(ch) => ch
                            .username()
                            .map(|u| format!("https://t.me/{u}/{}", msg.id())),
                        _ => None,
                    };
                    let v = serde_json::json!({
                        "peer": peer_id,
                        "message_id": msg_id,
                        "link": link,
                    });
                    return Ok(HandleResult {
                        data: serde_json::to_vec(&v).unwrap(),
                        event: None,
                    });
                }
            }
            Err(format!("message {msg_id} not found in {peer_id}"))
        }

        "tg_get_chat_link" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_get_chat_link: peer required")?;

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;

            let link = match &chat {
                grammers_client::types::Chat::User(user) => {
                    user.username().map(|u| format!("https://t.me/{u}"))
                }
                grammers_client::types::Chat::Group(group) => {
                    group.username().map(|u| format!("https://t.me/{u}"))
                }
                grammers_client::types::Chat::Channel(channel) => {
                    channel.username().map(|u| format!("https://t.me/{u}"))
                }
            };

            let v = serde_json::json!({
                "peer": peer_id,
                "link": link,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }

        "tg_export_history" => {
            let account_id = resolve_account(&params, &state.config);
            let peer_id = params
                .get("peer")
                .and_then(|v| v.as_str())
                .ok_or("tg_export_history: peer required")?;
            let limit = params
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(100)
                .min(1000);
            let output_path = params
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("/tmp/tg_export.txt");

            let client = state
                .pool
                .get(account_id)
                .ok_or_else(|| format!("account '{account_id}' not connected"))?;

            state
                .antiban
                .check(account_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat = find_chat(&client, peer_id).await?;
            let mut messages = client.iter_messages(&chat);
            let mut lines = Vec::new();
            let mut count = 0u64;

            while let Some(msg) = messages.next().await.map_err(|e| e.to_string())? {
                if count >= limit {
                    break;
                }
                count += 1;

                let sender = msg
                    .sender()
                    .map(|s| s.name().to_string())
                    .unwrap_or_default();
                let date = msg.date().format("%Y-%m-%d %H:%M").to_string();
                let text = msg.text();
                lines.push(format!("[{date}] {sender}: {text}"));
            }

            tokio::fs::write(output_path, lines.join("\n"))
                .await
                .map_err(|e| format!("failed to write file: {e}"))?;

            let v = serde_json::json!({
                "peer": peer_id,
                "exported": count,
                "path": output_path,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
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

async fn find_chat(
    client: &grammers_client::Client,
    peer: &str,
) -> Result<grammers_client::types::Chat, String> {
    let search = peer.strip_prefix('@').unwrap_or(peer);
    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
        let id_str = dialog.chat.id().to_string();
        let name = dialog.chat.name().to_string();
        let username = dialog.chat.username().unwrap_or("").to_string();
        if peer == "self" || peer == "me" {
            return Ok(dialog.chat.clone());
        }
        if id_str == peer
            || name.to_lowercase() == search.to_lowercase()
            || (!username.is_empty() && username.to_lowercase() == search.to_lowercase())
        {
            return Ok(dialog.chat.clone());
        }
    }
    Err(format!("peer '{peer}' not found"))
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
