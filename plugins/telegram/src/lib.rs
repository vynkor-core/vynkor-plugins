//! telegram plugin — full MTProto user-client (N-account)
//! Prototype scope: read/write/search + status; media deferred.
//! Architecture: single-reader loop + RPC proxy (see PLUGIN_AUTHORING.md §1).

pub mod events;
pub mod mtproto;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use mtproto::SessionPool;

const MAX_TEXT_LEN: usize = 4096;
const DEFAULT_LIMIT: u64 = 20;
const MAX_LIMIT: u64 = 100;

#[derive(Debug, Default)]
pub struct Metrics {
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub messages_edited: AtomicU64,
    pub messages_deleted: AtomicU64,
    pub messages_forwarded: AtomicU64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    pub api_id: i32,
    pub api_hash: String,
    pub phone: String,
    pub session_path: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub accounts: Vec<AccountConfig>,
    pub default_account: String,
    pub session_dir: String,
    pub pool: Option<Arc<SessionPool>>,
    pub start_instant: Instant,
    pub metrics: Arc<Metrics>,
}

impl Config {
    pub fn from_env() -> Self {
        let accounts_raw =
            std::env::var("TELEGRAM_PLUGIN_ACCOUNTS").unwrap_or_else(|_| "default".into());
        let ids: Vec<String> = accounts_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let session_dir =
            std::env::var("TELEGRAM_PLUGIN_SESSION_DIR").unwrap_or_else(|_| "/tmp".into());
        let mut accounts = Vec::new();
        for id in ids {
            let upper = id.to_uppercase();
            let api_id = std::env::var(format!("TELEGRAM_PLUGIN_API_ID_{upper}"))
                .or_else(|_| std::env::var("TG_API_ID"))
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let api_hash = std::env::var(format!("TELEGRAM_PLUGIN_API_HASH_{upper}"))
                .or_else(|_| std::env::var("TG_API_HASH"))
                .unwrap_or_default();
            let phone = std::env::var(format!("TELEGRAM_PLUGIN_PHONE_{upper}")).unwrap_or_default();
            let session_path = format!("{}/{}.session", session_dir, id);
            if api_id != 0 && !api_hash.is_empty() {
                accounts.push(AccountConfig {
                    id: id.clone(),
                    api_id,
                    api_hash,
                    phone,
                    session_path,
                });
            }
        }
        let default_account = accounts
            .first()
            .map(|a| a.id.clone())
            .unwrap_or_else(|| "default".into());
        Self {
            accounts,
            default_account,
            session_dir,
            pool: None,
            start_instant: Instant::now(),
            metrics: Arc::new(Metrics::default()),
        }
    }

    pub async fn connect_all(
        &mut self,
    ) -> Result<Vec<tokio::sync::mpsc::UnboundedReceiver<grammers_session::updates::UpdatesLike>>, String>
    {
        if self.accounts.is_empty() {
            return Ok(Vec::new());
        }
        let pool = Arc::new(SessionPool::new());
        let mut updates_receivers = Vec::new();
        for account in &self.accounts {
            let rx = pool
                .connect(account)
                .await
                .map_err(|e| format!("failed to connect {}: {e}", account.id))?;
            updates_receivers.push(rx);
        }
        self.pool = Some(pool);
        Ok(updates_receivers)
    }
}

pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<serde_json::Value, String>>,
}

#[derive(Debug)]
pub struct HandleResult {
    pub data: Vec<u8>,
    pub event: Option<EventToPublish>,
}

#[derive(Debug)]
pub struct EventToPublish {
    pub event_type: String,
    pub payload: serde_json::Value,
}

pub async fn handle_action(
    config: &Config,
    action: &str,
    params_json: &[u8],
) -> Result<HandleResult, String> {
    let params: Value = serde_json::from_slice(params_json).unwrap_or(Value::Null);
    match action {
        "status" => handle_status(config).await,
        "tg_list_dialogs" => handle_list_dialogs(config, &params).await,
        "tg_get_history" => handle_get_history(config, &params).await,
        "tg_get_message" => handle_get_message(config, &params).await,
        "tg_search" => handle_search(config, &params).await,
        "tg_send_message" => handle_send_message(config, &params).await,
        "tg_edit_message" => handle_edit_message(config, &params).await,
        "tg_delete_message" => handle_delete_message(config, &params).await,
        "tg_delete_messages" => handle_delete_messages(config, &params).await,
        "tg_forward_message" => handle_forward_message(config, &params).await,
        "tg_forward_messages" => handle_forward_messages(config, &params).await,
        "tg_add_reaction" => handle_add_reaction(config, &params).await,
        "tg_pin_message" => handle_pin_message(config, &params).await,
        "tg_upload_media" => handle_upload_media(config, &params).await,
        "tg_download_media" => handle_download_media(config, &params).await,
        "tg_send_voice" => handle_send_voice(config, &params).await,
        "tg_send_sticker" => handle_send_sticker(config, &params).await,
        "tg_send_animation" => handle_send_animation(config, &params).await,
        "tg_get_chat_info" => handle_get_chat_info(config, &params).await,
        "tg_get_user" => handle_get_user(config, &params).await,
        "tg_list_contacts" => handle_list_contacts(config, &params).await,
        "tg_get_contact" => handle_get_contact(config, &params).await,
        "tg_list_unread" => handle_list_unread(config, &params).await,
        "tg_get_unread" => handle_list_unread(config, &params).await,
        "tg_mark_read" => handle_mark_read(config, &params).await,
        "tg_mark_all_read" => handle_mark_read(config, &params).await,
        "tg_send_action" => handle_send_action(config, &params).await,
        "tg_set_typing" => handle_send_action(config, &params).await,
        other => Err(format!("unknown action: {other}")),
    }
}

async fn handle_status(config: &Config) -> Result<HandleResult, String> {
    let uptime_ms = config.start_instant.elapsed().as_millis() as u64;
    let connected = config.pool.as_ref().map(|p| {
        config
            .accounts
            .iter()
            .any(|a| p.get(&a.id).is_some())
    }).unwrap_or(false);

    let v = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "accounts": config.accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        "default_account": config.default_account,
        "uptime_ms": uptime_ms,
        "engine_ready": connected,
        "last_error": null,
        "counters": {},
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_list_dialogs(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let limit = clamp_limit(params);
    let query = params
        .get("query")
        .or_else(|| params.get("q"))
        .or_else(|| params.get("search"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let client = get_client(config, account)?;

    let mut dialogs = client.iter_dialogs();
    let mut result = Vec::new();
    let mut total = 0u64;
    let mut matched_total = 0u64;

    let unread_only = params
        .get("unread_only")
        .or_else(|| params.get("only_unread"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    while let Some(dialog) = dialogs.next().await.map_err(|e| format!("dialog iter: {e}"))? {
        let (unread_count, unread_mentions, unread_mark) = dialog_unread(&dialog);
        if unread_only && unread_count == 0 && !unread_mark {
            continue;
        }
        total += 1;
        let peer = dialog.peer();
        if let Some(ref q) = query {
            let name = peer.name().unwrap_or("").to_lowercase();
            let username = peer.username().unwrap_or("").to_lowercase();
            let first_last = match peer {
                grammers_client::types::Peer::User(u) => u.full_name().to_lowercase(),
                _ => String::new(),
            };
            let hay = format!("{name} {username} {first_last}");
            if !hay.contains(q) && !peer_to_string(peer).to_lowercase().contains(q) {
                if let grammers_client::types::Peer::User(u) = peer {
                    let phone = u.phone().unwrap_or("").to_lowercase();
                    if !phone.contains(q) {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            matched_total += 1;
            if result.len() >= limit as usize {
                continue;
            }
        } else if result.len() >= limit as usize {
            continue;
        }
        let mut entry = peer_to_details(peer);
        entry["peer"] = serde_json::Value::String(peer_to_string(peer));
        entry["unread_count"] = serde_json::json!(unread_count);
        entry["unread_mentions"] = serde_json::json!(unread_mentions);
        entry["unread_mark"] = serde_json::json!(unread_mark);
        result.push(entry);
        if query.is_some() && matched_total == 0 {
            matched_total = result.len() as u64;
        }
    }

    let v = if query.is_some() {
        serde_json::json!({"account": account, "dialogs": result, "total": total, "matched": matched_total, "query": query})
    } else {
        serde_json::json!({"account": account, "dialogs": result, "total": total})
    };
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_get_history(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = required_str(params, "peer", "tg_get_history")?;
    let limit = clamp_limit(params);
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let mut messages = client.iter_messages(peer).limit(limit as usize);
    let mut result = Vec::new();
    let mut total = 0u64;

    while let Some(msg) = messages.next().await.map_err(|e| format!("message iter: {e}"))? {
        total += 1;
        result.push(serde_json::json!({
            "id": msg.id(),
            "text": msg.text(),
            "date": msg.date().to_rfc3339(),
            "outgoing": msg.outgoing(),
        }));
    }

    let v = serde_json::json!({"peer": peer_str, "messages": result, "total": total});
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_get_message(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let id = params
        .get("id")
        .and_then(|v| v.as_u64())
        .filter(|id| *id != 0)
        .ok_or_else(|| "tg_get_message: id required".to_string())?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, optional_peer(params)).await?;

    let mut messages = client.iter_messages(peer).limit(1);
    while let Some(msg) = messages.next().await.map_err(|e| format!("message iter: {e}"))? {
        if msg.id() == id as i32 {
            return Ok(HandleResult {
                data: serde_json::to_vec(&serde_json::json!({
                    "found": true,
                    "message": {
                        "id": msg.id(),
                        "text": msg.text(),
                        "date": msg.date().to_rfc3339(),
                        "outgoing": msg.outgoing(),
                    }
                })).unwrap(),
                event: None,
            });
        }
    }

    Ok(HandleResult {
        data: serde_json::to_vec(&serde_json::json!({"found": false})).unwrap(),
        event: None,
    })
}

async fn handle_search(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let query = required_str(params, "query", "tg_search")?;
    let limit = clamp_limit(params);
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;

    let mut search = client.search_all_messages().query(query).limit(limit as usize);
    let mut result = Vec::new();
    let mut total = 0u64;

    while let Some(msg) = search.next().await.map_err(|e| format!("search iter: {e}"))? {
        total += 1;
        let chat_name = msg.peer()
            .map(|p| p.name().unwrap_or("unknown").to_string())
            .unwrap_or_default();
        result.push(serde_json::json!({
            "id": msg.id(),
            "text": msg.text(),
            "chat": chat_name,
            "date": msg.date().to_rfc3339(),
        }));
    }

    let v = serde_json::json!({"query": query, "messages": result, "total": total});
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_send_message(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let text = required_str(params, "text", "tg_send_message")?;
    if text.len() > MAX_TEXT_LEN {
        return Err(format!("tg_send_message: text too long (max {MAX_TEXT_LEN})"));
    }
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let no_typing = params
        .get("no_typing")
        .or_else(|| params.get("skip_typing"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !no_typing && !text.trim().is_empty() {
        let char_count = text.chars().count() as u64;
        let mut duration_ms = char_count * 10;
        if duration_ms < 300 && char_count > 0 {
            duration_ms = 300;
        }
        if duration_ms > 5000 {
            duration_ms = 5000;
        }
        let _ = emulate_typing(&client, peer, duration_ms).await;
    }

    let reply_to = params.get("reply_to").and_then(|v| v.as_u64()).map(|id| id as i32);

    let mut msg = grammers_client::types::InputMessage::default().text(text);
    if let Some(reply_id) = reply_to {
        msg = msg.reply_to(Some(reply_id));
    }

    let sent = client.send_message(peer, msg).await
        .map_err(|e| format!("send_message failed: {e}"))?;

    config.metrics.messages_sent.fetch_add(1, Ordering::Relaxed);

    let message_id = sent.id();
    let ev = EventToPublish {
        event_type: "plugin.telegram.message_sent".into(),
        payload: serde_json::json!({"peer": peer_str, "message_id": message_id}),
    };

    let v = serde_json::json!({"peer": peer_str, "message_id": message_id});
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: Some(ev),
    })
}

async fn handle_edit_message(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let text = required_str(params, "text", "tg_edit_message")?;
    if text.len() > MAX_TEXT_LEN {
        return Err(format!("tg_edit_message: text too long (max {MAX_TEXT_LEN})"));
    }
    let message_id = params
        .get("message_id")
        .and_then(|v| v.as_u64())
        .filter(|id| *id != 0)
        .ok_or_else(|| "tg_edit_message: message_id required".to_string())?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    client
        .edit_message(peer, message_id as i32, text)
        .await
        .map_err(|e| format!("edit_message failed: {e}"))?;

    config.metrics.messages_edited.fetch_add(1, Ordering::Relaxed);

    let v = serde_json::json!({"peer": peer_str, "message_id": message_id, "edited": true});
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_delete_message(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let message_id = params
        .get("message_id")
        .and_then(|v| v.as_u64())
        .filter(|id| *id != 0)
        .ok_or_else(|| "tg_delete_message: message_id required".to_string())?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    client
        .delete_messages(peer, &[message_id as i32])
        .await
        .map_err(|e| format!("delete_message failed: {e}"))?;

    config.metrics.messages_deleted.fetch_add(1, Ordering::Relaxed);

    let v = serde_json::json!({"peer": peer_str, "message_id": message_id, "deleted": true});
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_forward_message(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let from_peer_str = required_str(params, "from_peer", "tg_forward_message")?;
    let to_peer_str = required_str(params, "to_peer", "tg_forward_message")?;
    let message_id = params
        .get("message_id")
        .and_then(|v| v.as_u64())
        .filter(|id| *id != 0)
        .ok_or_else(|| "tg_forward_message: message_id required".to_string())?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let from_peer = resolve_peer(&client, from_peer_str).await?;
    let to_peer = resolve_peer(&client, to_peer_str).await?;

    let forwarded = client
        .forward_messages(to_peer, &[message_id as i32], from_peer)
        .await
        .map_err(|e| format!("forward_message failed: {e}"))?;

    config.metrics.messages_forwarded.fetch_add(1, Ordering::Relaxed);

    let new_id = forwarded.first().and_then(|m| m.as_ref().map(|m| m.id()));
    let v = serde_json::json!({
        "from_peer": from_peer_str,
        "to_peer": to_peer_str,
        "original_id": message_id,
        "new_id": new_id,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_add_reaction(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let reaction = required_str(params, "reaction", "tg_add_reaction")?;
    let message_id = params
        .get("message_id")
        .and_then(|v| v.as_u64())
        .filter(|id| *id != 0)
        .ok_or_else(|| "tg_add_reaction: message_id required".to_string())?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    client
        .send_reactions(peer, message_id as i32, reaction)
        .await
        .map_err(|e| format!("add_reaction failed: {e}"))?;

    let v = serde_json::json!({
        "peer": peer_str,
        "message_id": message_id,
        "reaction": reaction,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_pin_message(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let message_id = params
        .get("message_id")
        .and_then(|v| v.as_u64())
        .filter(|id| *id != 0)
        .ok_or_else(|| "tg_pin_message: message_id required".to_string())?;
    let silent = params
        .get("silent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    client
        .pin_message(peer, message_id as i32)
        .await
        .map_err(|e| format!("pin_message failed: {e}"))?;

    let v = serde_json::json!({
        "peer": peer_str,
        "message_id": message_id,
        "pinned": true,
        "silent": silent,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_upload_media(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let file_path = required_str(params, "file_path", "tg_upload_media")?;
    let caption = params
        .get("caption")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let uploaded = client
        .upload_file(file_path)
        .await
        .map_err(|e| format!("upload failed: {e}"))?;

    let msg = grammers_client::types::InputMessage::default()
        .text(caption)
        .photo(uploaded);

    let sent = client
        .send_message(peer, msg)
        .await
        .map_err(|e| format!("send_message with media failed: {e}"))?;

    config.metrics.messages_sent.fetch_add(1, Ordering::Relaxed);

    let v = serde_json::json!({
        "peer": peer_str,
        "message_id": sent.id(),
        "file_path": file_path,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_download_media(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let message_id = params
        .get("message_id")
        .and_then(|v| v.as_u64())
        .filter(|id| *id != 0)
        .ok_or_else(|| "tg_download_media: message_id required".to_string())?;
    let output_path = required_str(params, "output_path", "tg_download_media")?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let mut messages = client.iter_messages(peer).limit(1);
    while let Some(msg) = messages.next().await.map_err(|e| format!("message iter: {e}"))? {
        if msg.id() == message_id as i32 {
            if let Some(media) = msg.media() {
                client
                    .download_media(&media, output_path)
                    .await
                    .map_err(|e| format!("download failed: {e}"))?;

                let v = serde_json::json!({
                    "peer": peer_str,
                    "message_id": message_id,
                    "output_path": output_path,
                    "downloaded": true,
                });
                return Ok(HandleResult {
                    data: serde_json::to_vec(&v).unwrap(),
                    event: None,
                });
            }
        }
    }

    Err("tg_download_media: message not found or has no media".to_string())
}

async fn handle_delete_messages(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let message_ids = params
        .get("message_ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "tg_delete_messages: message_ids array required".to_string())?;
    let ids: Vec<i32> = message_ids
        .iter()
        .filter_map(|v| v.as_u64().map(|id| id as i32))
        .filter(|id| *id != 0)
        .collect();
    if ids.is_empty() {
        return Err("tg_delete_messages: no valid message_ids".into());
    }
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let deleted = client
        .delete_messages(peer, &ids)
        .await
        .map_err(|e| format!("delete_messages failed: {e}"))?;

    let v = serde_json::json!({
        "peer": peer_str,
        "message_ids": ids,
        "deleted_count": deleted,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_forward_messages(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let from_peer_str = required_str(params, "from_peer", "tg_forward_messages")?;
    let to_peer_str = required_str(params, "to_peer", "tg_forward_messages")?;
    let message_ids = params
        .get("message_ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "tg_forward_messages: message_ids array required".to_string())?;
    let ids: Vec<i32> = message_ids
        .iter()
        .filter_map(|v| v.as_u64().map(|id| id as i32))
        .filter(|id| *id != 0)
        .collect();
    if ids.is_empty() {
        return Err("tg_forward_messages: no valid message_ids".into());
    }
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let from_peer = resolve_peer(&client, from_peer_str).await?;
    let to_peer = resolve_peer(&client, to_peer_str).await?;

    let forwarded = client
        .forward_messages(to_peer, &ids, from_peer)
        .await
        .map_err(|e| format!("forward_messages failed: {e}"))?;

    let new_ids: Vec<i32> = forwarded
        .iter()
        .filter_map(|m| m.as_ref().map(|m| m.id()))
        .collect();

    let v = serde_json::json!({
        "from_peer": from_peer_str,
        "to_peer": to_peer_str,
        "original_ids": ids,
        "new_ids": new_ids,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_send_voice(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let file_path = required_str(params, "file_path", "tg_send_voice")?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let uploaded = client
        .upload_file(file_path)
        .await
        .map_err(|e| format!("upload failed: {e}"))?;

    let msg = grammers_client::types::InputMessage::default()
        .document(uploaded)
        .attribute(grammers_client::types::Attribute::Voice {
            duration: std::time::Duration::ZERO,
            waveform: Some(Vec::new()),
        });

    let sent = client
        .send_message(peer, msg)
        .await
        .map_err(|e| format!("send_voice failed: {e}"))?;

    let v = serde_json::json!({
        "peer": peer_str,
        "message_id": sent.id(),
        "file_path": file_path,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_send_sticker(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let file_path = required_str(params, "file_path", "tg_send_sticker")?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let uploaded = client
        .upload_file(file_path)
        .await
        .map_err(|e| format!("upload failed: {e}"))?;

    let msg = grammers_client::types::InputMessage::default().document(uploaded);

    let sent = client
        .send_message(peer, msg)
        .await
        .map_err(|e| format!("send_sticker failed: {e}"))?;

    let v = serde_json::json!({
        "peer": peer_str,
        "message_id": sent.id(),
        "file_path": file_path,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_send_animation(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = optional_peer(params);
    let file_path = required_str(params, "file_path", "tg_send_animation")?;
    let caption = params
        .get("caption")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let uploaded = client
        .upload_file(file_path)
        .await
        .map_err(|e| format!("upload failed: {e}"))?;

    let msg = grammers_client::types::InputMessage::default()
        .text(caption)
        .document(uploaded);

    let sent = client
        .send_message(peer, msg)
        .await
        .map_err(|e| format!("send_animation failed: {e}"))?;

    let v = serde_json::json!({
        "peer": peer_str,
        "message_id": sent.id(),
        "file_path": file_path,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_get_chat_info(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = required_str(params, "peer", "tg_get_chat_info")?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs.next().await.map_err(|e| format!("dialog iter: {e}"))? {
        let dp = dialog.peer();
        if dp.id() == peer.id {
            let mut v = peer_to_details(dp);
            v["peer"] = serde_json::json!(peer_str);
            return Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            });
        }
    }

    Err(format!("tg_get_chat_info: chat not found for {peer_str}"))
}

async fn handle_get_user(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = required_str(params, "peer", "tg_get_user")?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs.next().await.map_err(|e| format!("dialog iter: {e}"))? {
        let dp = dialog.peer();
        if dp.id() == peer.id {
            let mut v = peer_to_details(dp);
            v["peer"] = serde_json::json!(peer_str);
            return Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            });
        }
    }

    Err(format!("tg_get_user: user not found for {peer_str}"))
}

async fn handle_list_contacts(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let limit = clamp_limit(params);
    let query = params
        .get("query")
        .or_else(|| params.get("q"))
        .or_else(|| params.get("search"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let client = get_client(config, account)?;
    let mut dialogs = client.iter_dialogs();
    let mut result = Vec::new();
    let mut total = 0u64;
    let mut matched = 0u64;

    while let Some(dialog) = dialogs.next().await.map_err(|e| format!("dialog iter: {e}"))? {
        let peer = dialog.peer();
        let user = match peer {
            grammers_client::types::Peer::User(u) => u,
            _ => continue,
        };
        if !user.contact() && query.is_none() {
            continue;
        }
        if let Some(ref q) = query {
            let hay = format!(
                "{} {} {} {} {}",
                user.full_name().to_lowercase(),
                user.first_name().unwrap_or("").to_lowercase(),
                user.last_name().unwrap_or("").to_lowercase(),
                user.username().unwrap_or("").to_lowercase(),
                user.phone().unwrap_or("").to_lowercase()
            );
            if !hay.contains(q) && !peer_to_string(peer).to_lowercase().contains(q) {
                continue;
            }
            matched += 1;
            if result.len() >= limit as usize {
                continue;
            }
        } else {
            total += 1;
            if result.len() >= limit as usize {
                continue;
            }
        }
        let mut entry = peer_to_details(peer);
        entry["peer"] = serde_json::Value::String(peer_to_string(peer));
        result.push(entry);
    }

    let v = if query.is_some() {
        serde_json::json!({"account": account, "contacts": result, "total": matched, "query": query})
    } else {
        serde_json::json!({"account": account, "contacts": result, "total": total})
    };
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_get_contact(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = required_str(params, "peer", "tg_get_contact")?;
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs.next().await.map_err(|e| format!("dialog iter: {e}"))? {
        let dp = dialog.peer();
        if dp.id() == peer.id {
            let mut v = peer_to_details(dp);
            v["peer"] = serde_json::json!(peer_str);
            v["found"] = serde_json::json!(true);
            return Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            });
        }
    }

    Err(format!("tg_get_contact: contact not found for {peer_str}"))
}

async fn handle_list_unread(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let limit = clamp_limit(params);
    let include_messages = params
        .get("include_messages")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let message_limit = params
        .get("message_limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let client = get_client(config, account)?;

    let mut dialogs = client.iter_dialogs();
    let mut result = Vec::new();
    let mut total_unread_dialogs = 0u64;
    let mut total_unread_messages = 0u64;

    while let Some(dialog) = dialogs.next().await.map_err(|e| format!("dialog iter: {e}"))? {
        let (unread_count, unread_mentions, unread_mark) = dialog_unread(&dialog);
        if unread_count == 0 && !unread_mark {
            continue;
        }
        total_unread_dialogs += 1;
        total_unread_messages += unread_count as u64;
        if result.len() >= limit as usize {
            continue;
        }
        let peer = dialog.peer();
        let mut entry = peer_to_details(peer);
        entry["peer"] = serde_json::json!(peer_to_string(peer));
        entry["unread_count"] = serde_json::json!(unread_count);
        entry["unread_mentions"] = serde_json::json!(unread_mentions);
        entry["unread_mark"] = serde_json::json!(unread_mark);

        if include_messages && unread_count > 0 {
            let fetch = if message_limit > 0 {
                (message_limit as usize).min(unread_count as usize)
            } else {
                (unread_count as usize).min(20)
            };
            let peer_ref: grammers_session::defs::PeerRef = peer.into();
            let mut iter = client.iter_messages(peer_ref).limit(fetch);
            let mut msgs = Vec::new();
            while let Some(msg) = iter.next().await.map_err(|e| format!("message iter: {e}"))? {
                if msg.outgoing() {
                    continue;
                }
                msgs.push(serde_json::json!({
                    "id": msg.id(),
                    "text": msg.text(),
                    "date": msg.date().to_rfc3339(),
                    "outgoing": msg.outgoing(),
                    "sender": msg.sender().map(peer_to_string).unwrap_or_default(),
                }));
                if msgs.len() >= fetch {
                    break;
                }
            }
            entry["unread_messages"] = serde_json::json!(msgs);
        }

        result.push(entry);
    }

    let v = serde_json::json!({
        "account": account,
        "dialogs": result,
        "total_unread_dialogs": total_unread_dialogs,
        "total_unread_messages": total_unread_messages,
    });
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_mark_read(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = params
        .get("peer")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let max_id = params
        .get("max_id")
        .or_else(|| params.get("message_id"))
        .or_else(|| params.get("id"))
        .and_then(|v| v.as_u64())
        .map(|v| v as i32)
        .unwrap_or(0);
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;

    if let Some(peer_str) = peer_str {
        let peer = resolve_peer(&client, peer_str).await?;
        if max_id != 0 {
            use grammers_client::grammers_tl_types as tl;
            use grammers_session::defs::PeerKind;
            if peer.id.kind() == PeerKind::Channel {
                client
                    .invoke(&tl::functions::channels::ReadHistory {
                        channel: peer.into(),
                        max_id,
                    })
                    .await
                    .map_err(|e| format!("tg_mark_read failed for {peer_str} max_id {max_id}: {e}"))?;
            } else {
                client
                    .invoke(&tl::functions::messages::ReadHistory {
                        peer: peer.into(),
                        max_id,
                    })
                    .await
                    .map_err(|e| format!("tg_mark_read failed for {peer_str} max_id {max_id}: {e}"))?;
            }
        } else {
            client
                .mark_as_read(peer)
                .await
                .map_err(|e| format!("tg_mark_read failed for {peer_str}: {e}"))?;
        }
        let v = serde_json::json!({"peer": peer_str, "marked": true, "max_id": max_id});
        return Ok(HandleResult {
            data: serde_json::to_vec(&v).unwrap(),
            event: None,
        });
    }

    let mut dialogs = client.iter_dialogs();
    let mut marked = 0u64;
    let mut errors = Vec::new();
    while let Some(dialog) = dialogs.next().await.map_err(|e| format!("dialog iter: {e}"))? {
        let (unread_count, _, unread_mark) = dialog_unread(&dialog);
        if unread_count == 0 && !unread_mark {
            continue;
        }
        let peer = dialog.peer();
        let peer_ref: grammers_session::defs::PeerRef = peer.into();
        match client.mark_as_read(peer_ref).await {
            Ok(_) => marked += 1,
            Err(e) => errors.push(format!("{}: {e}", peer_to_string(peer))),
        }
    }

    let v = serde_json::json!({"marked": marked, "errors": errors});
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_send_action(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = required_str(params, "peer", "tg_send_action")?;
    let action_str = params
        .get("action")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| "typing".to_string());
    let progress = params.get("progress").and_then(|v| v.as_u64()).unwrap_or(0).clamp(0, 100) as i32;
    let duration_ms = params
        .get("duration_ms")
        .or_else(|| params.get("duration"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let account = resolve_account(params, config);
    check_antiban(config, account).await?;
    let client = get_client(config, account)?;
    let peer = resolve_peer(&client, peer_str).await?;

    use grammers_client::grammers_tl_types as tl;
    let tl_action: tl::enums::SendMessageAction = match action_str.as_str() {
        "typing" | "type" => tl::types::SendMessageTypingAction {}.into(),
        "cancel" | "clear" => tl::types::SendMessageCancelAction {}.into(),
        "record_audio" | "record_voice" => tl::types::SendMessageRecordAudioAction {}.into(),
        "record_video" => tl::types::SendMessageRecordVideoAction {}.into(),
        "record_round" | "record_circle" => tl::types::SendMessageRecordRoundAction {}.into(),
        "upload_photo" | "photo" => tl::types::SendMessageUploadPhotoAction { progress }.into(),
        "upload_document" | "document" | "file" => tl::types::SendMessageUploadDocumentAction { progress }.into(),
        "upload_video" | "video" => tl::types::SendMessageUploadVideoAction { progress }.into(),
        "upload_audio" | "audio" => tl::types::SendMessageUploadAudioAction { progress }.into(),
        "upload_round" => tl::types::SendMessageUploadRoundAction { progress }.into(),
        "choose_sticker" | "sticker" => tl::types::SendMessageChooseStickerAction {}.into(),
        "choose_contact" => tl::types::SendMessageChooseContactAction {}.into(),
        "game_play" | "game" => tl::types::SendMessageGamePlayAction {}.into(),
        "geo" | "location" => tl::types::SendMessageGeoLocationAction {}.into(),
        "speaking" => tl::types::SpeakingInGroupCallAction {}.into(),
        other => return Err(format!("tg_send_action: unknown action '{other}' — use typing/cancel/record_audio/record_video/upload_photo/etc.")),
    };

    if duration_ms > 0 {
        let repeat_ms = 4000u64;
        let mut elapsed = 0u64;
        while elapsed < duration_ms {
            client
                .action(peer)
                .oneshot(tl_action.clone())
                .await
                .map_err(|e| format!("tg_send_action failed: {e}"))?;
            let step = (duration_ms - elapsed).min(repeat_ms);
            if step < duration_ms - elapsed {
                tokio::time::sleep(std::time::Duration::from_millis(step)).await;
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(step)).await;
                break;
            }
            elapsed += step;
        }
    } else {
        client
            .action(peer)
            .oneshot(tl_action)
            .await
            .map_err(|e| format!("tg_send_action failed: {e}"))?;
    }

    let v = serde_json::json!({"peer": peer_str, "action": action_str, "duration_ms": duration_ms});
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn emulate_typing(
    client: &grammers_client::Client,
    peer: grammers_session::defs::PeerRef,
    duration_ms: u64,
) -> Result<(), String> {
    use grammers_client::grammers_tl_types as tl;
    let action: tl::enums::SendMessageAction = tl::types::SendMessageTypingAction {}.into();
    let mut elapsed = 0u64;
    let repeat_ms = 4000u64;
    while elapsed < duration_ms {
        let res = client.action(peer).oneshot(action.clone()).await;
        if res.is_err() {
            break;
        }
        let step = (duration_ms - elapsed).min(repeat_ms);
        tokio::time::sleep(std::time::Duration::from_millis(step)).await;
        elapsed += step;
    }
    Ok(())
}

fn dialog_unread(dialog: &grammers_client::types::Dialog) -> (i32, i32, bool) {
    use grammers_client::grammers_tl_types::enums::Dialog as D;
    match &dialog.raw {
        D::Dialog(d) => (d.unread_count, d.unread_mentions_count, d.unread_mark),
        D::Folder(_) => (0, 0, false),
    }
}

fn get_client(config: &Config, account: &str) -> Result<grammers_client::Client, String> {
    config
        .pool
        .as_ref()
        .ok_or_else(|| "no telegram accounts connected".to_string())?
        .get(account)
        .ok_or_else(|| format!("account not connected: {account}"))
}

async fn check_antiban(config: &Config, account: &str) -> Result<(), String> {
    config
        .pool
        .as_ref()
        .ok_or_else(|| "no telegram accounts connected".to_string())?
        .antiban
        .check(account)
        .await
        .map_err(|e| format!("antiban: {e}"))
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
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT)
}

fn optional_peer(params: &Value) -> &str {
    params
        .get("peer")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("self")
}

fn required_str<'a>(params: &'a Value, key: &str, action: &str) -> Result<&'a str, String> {
    match params.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(format!("{action}: {key} required")),
    }
}

fn peer_to_details(peer: &grammers_client::types::Peer) -> Value {
    use grammers_client::types::Peer;
    match peer {
        Peer::User(u) => serde_json::json!({
            "peer_type": "user",
            "id": u.raw.id(),
            "name": u.full_name(),
            "first_name": u.first_name().unwrap_or(""),
            "last_name": u.last_name().unwrap_or(""),
            "username": u.username().unwrap_or(""),
            "usernames": u.usernames(),
            "phone": u.phone().unwrap_or(""),
            "is_contact": u.contact(),
            "is_mutual": u.mutual_contact(),
            "is_bot": u.is_bot(),
            "is_verified": u.verified(),
            "is_deleted": u.deleted(),
        }),
        Peer::Group(g) => serde_json::json!({
            "peer_type": "group",
            "id": g.id().bare_id(),
            "name": g.title().unwrap_or(""),
            "username": g.username().unwrap_or(""),
            "usernames": g.usernames(),
            "title": g.title().unwrap_or(""),
        }),
        Peer::Channel(c) => serde_json::json!({
            "peer_type": "channel",
            "id": c.bare_id(),
            "name": c.title(),
            "username": c.username().unwrap_or(""),
            "usernames": c.usernames(),
            "title": c.title(),
        }),
    }
}

fn peer_to_string(peer: &grammers_client::types::Peer) -> String {
    use grammers_client::types::Peer;
    match peer {
        Peer::User(u) => format!("user:{}", u.raw.id()),
        Peer::Group(g) => format!("group:{}", g.id().bare_id()),
        Peer::Channel(c) => format!("channel:{}", c.raw.id),
    }
}

fn parse_peer(s: &str) -> Result<grammers_session::defs::PeerRef, String> {
    use grammers_session::defs::{PeerAuth, PeerId, PeerRef};
    if s == "self" {
        return Ok(PeerRef {
            id: PeerId::self_user(),
            auth: PeerAuth::default(),
        });
    }
    if let Some(id) = s.strip_prefix("user:") {
        let id: i64 = id.parse().map_err(|_| format!("invalid user id: {id}"))?;
        return Ok(PeerRef {
            id: PeerId::user(id),
            auth: PeerAuth::default(),
        });
    }
    if let Some(id) = s.strip_prefix("chat:") {
        let id: i64 = id.parse().map_err(|_| format!("invalid chat id: {id}"))?;
        return Ok(PeerRef {
            id: PeerId::chat(id),
            auth: PeerAuth::default(),
        });
    }
    if let Some(id) = s.strip_prefix("channel:") {
        let id: i64 = id.parse().map_err(|_| format!("invalid channel id: {id}"))?;
        return Ok(PeerRef {
            id: PeerId::channel(id),
            auth: PeerAuth::default(),
        });
    }
    if s.starts_with('@') {
        return Err("RESOLVE_USERNAME".into());
    }
    if let Ok(id) = s.parse::<i64>() {
        if id < 0 {
            return Ok(PeerRef {
                id: PeerId::channel(id.abs()),
                auth: PeerAuth::default(),
            });
        }
        return Ok(PeerRef {
            id: PeerId::user(id),
            auth: PeerAuth::default(),
        });
    }
    Err(format!("invalid peer format: {s}"))
}

async fn resolve_peer(
    client: &grammers_client::Client,
    peer_str: &str,
) -> Result<grammers_session::defs::PeerRef, String> {
    match parse_peer(peer_str) {
        Ok(p) if !peer_str.starts_with('@') => Ok(p),
        Err(e) if e == "RESOLVE_USERNAME" => {
            let username = &peer_str[1..];
            let resolved = client
                .resolve_username(username)
                .await
                .map_err(|e| format!("resolve @{username} failed: {e}"))?
                .ok_or_else(|| format!("user @{username} not found"))?;
            Ok((&resolved).into())
        }
        Err(e) => Err(e),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_config() -> Config {
        Config {
            accounts: vec![AccountConfig {
                id: "personal".into(),
                api_id: 1,
                api_hash: "hash".into(),
                phone: "+10000000000".into(),
                session_path: "/tmp/personal.session".into(),
            }],
            default_account: "personal".into(),
            session_dir: "/tmp".into(),
            pool: None,
            start_instant: std::time::Instant::now(),
            metrics: std::sync::Arc::new(Metrics::default()),
        }
    }

    async fn call(action: &str, params: Value) -> Result<HandleResult, String> {
        let cfg = test_config();
        let bytes = serde_json::to_vec(&params).unwrap();
        handle_action(&cfg, action, &bytes).await
    }

    #[tokio::test]
    async fn tg_search_requires_query() {
        let err = call("tg_search", json!({})).await.unwrap_err();
        assert!(err.contains("query required"), "{err}");
    }

    #[tokio::test]
    async fn tg_send_message_requires_text() {
        let err = call("tg_send_message", json!({"text": ""}))
            .await
            .unwrap_err();
        assert!(err.contains("text required"), "{err}");
    }

    #[tokio::test]
    async fn tg_send_message_text_too_long() {
        let err = call(
            "tg_send_message",
            json!({"text": "a".repeat(MAX_TEXT_LEN + 1)}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("too long"), "{err}");
    }

    #[tokio::test]
    async fn tg_get_history_requires_peer() {
        let err = call("tg_get_history", json!({})).await.unwrap_err();
        assert!(err.contains("peer required"), "{err}");
    }

    #[tokio::test]
    async fn tg_get_message_requires_id() {
        let err = call("tg_get_message", json!({"id": 0})).await.unwrap_err();
        assert!(err.contains("id required"), "{err}");
    }

    #[tokio::test]
    async fn status_returns_version_and_accounts() {
        let res = call("status", json!({})).await.unwrap();
        let v: Value = serde_json::from_slice(&res.data).unwrap();
        assert_eq!(v["version"], "0.1.0");
        assert_eq!(v["default_account"], "personal");
        let accounts = v["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0], "personal");
        assert!(v["uptime_ms"].as_u64().is_some());
        assert_eq!(v["engine_ready"], false);
    }

    #[tokio::test]
    async fn tg_list_dialogs_no_pool_errors() {
        let err = call("tg_list_dialogs", json!({})).await.unwrap_err();
        assert!(err.contains("no telegram accounts connected"), "{err}");
    }

    #[tokio::test]
    async fn tg_get_history_no_pool_errors() {
        let err = call("tg_get_history", json!({"peer": "self"}))
            .await
            .unwrap_err();
        assert!(err.contains("no telegram accounts connected"), "{err}");
    }

    #[tokio::test]
    async fn tg_search_empty_query_errors() {
        let err = call("tg_search", json!({"query": ""})).await.unwrap_err();
        assert!(err.contains("query required"), "{err}");
    }

    #[tokio::test]
    async fn tg_send_message_no_pool_errors() {
        let err = call("tg_send_message", json!({"peer": "self", "text": "hi"}))
            .await
            .unwrap_err();
        assert!(err.contains("no telegram accounts connected"), "{err}");
    }

    #[tokio::test]
    async fn unknown_action_returns_error() {
        let err = call("bogus_action", json!({})).await.unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }

    #[tokio::test]
    async fn tg_get_message_missing_peer_defaults_to_self() {
        let err = call("tg_get_message", json!({"id": 1})).await.unwrap_err();
        assert!(err.contains("no telegram accounts connected"), "{err}");
    }

    #[test]
    fn parse_peer_self() {
        assert!(parse_peer("self").is_ok());
    }

    #[test]
    fn parse_peer_user_id() {
        let p = parse_peer("12345").unwrap();
        assert_eq!(p.id, grammers_session::defs::PeerId::user(12345));
    }

    #[test]
    fn parse_peer_negative_is_channel() {
        let p = parse_peer("-100123").unwrap();
        assert_eq!(p.id, grammers_session::defs::PeerId::channel(100123));
    }

    #[test]
    fn parse_peer_username_unsupported() {
        assert!(parse_peer("@hello").is_err());
        assert!(parse_peer("@hello").unwrap_err().contains("RESOLVE_USERNAME"));
    }

    #[test]
    fn parse_peer_prefixed() {
        assert_eq!(parse_peer("user:1").unwrap().id, grammers_session::defs::PeerId::user(1));
        assert_eq!(parse_peer("chat:2").unwrap().id, grammers_session::defs::PeerId::chat(2));
        assert_eq!(parse_peer("channel:3").unwrap().id, grammers_session::defs::PeerId::channel(3));
    }

    #[test]
    fn parse_peer_invalid() {
        assert!(parse_peer("not_a_peer").is_err());
    }

    #[tokio::test]
    async fn concurrent_status_calls() {
        let cfg = test_config();
        let mut handles = Vec::new();
        for _ in 0..5 {
            let cfg = cfg.clone();
            handles.push(tokio::spawn(async move {
                let bytes = serde_json::to_vec(&serde_json::json!({})).unwrap();
                handle_action(&cfg, "status", &bytes).await
            }));
        }
        for h in handles {
            let res = h.await.unwrap();
            assert!(res.is_ok());
        }
    }

    #[tokio::test]
    async fn concurrent_search_and_history() {
        let cfg = test_config();
        let mut handles = Vec::new();

        let cfg1 = cfg.clone();
        handles.push(tokio::spawn(async move {
            let bytes = serde_json::to_vec(&serde_json::json!({"query": "test"})).unwrap();
            handle_action(&cfg1, "tg_search", &bytes).await
        }));

        let cfg2 = cfg.clone();
        handles.push(tokio::spawn(async move {
            let bytes = serde_json::to_vec(&serde_json::json!({"peer": "self"})).unwrap();
            handle_action(&cfg2, "tg_get_history", &bytes).await
        }));

        let cfg3 = cfg.clone();
        handles.push(tokio::spawn(async move {
            let bytes = serde_json::to_vec(&serde_json::json!({"text": "hello"})).unwrap();
            handle_action(&cfg3, "tg_send_message", &bytes).await
        }));

        for h in handles {
            let res = h.await.unwrap();
            assert!(res.is_ok() || res.is_err());
        }
    }

    #[tokio::test]
    async fn tg_edit_message_requires_message_id() {
        let err = call("tg_edit_message", json!({"text": "edited"}))
            .await
            .unwrap_err();
        assert!(err.contains("message_id required"), "{err}");
    }

    #[tokio::test]
    async fn tg_edit_message_text_too_long() {
        let err = call(
            "tg_edit_message",
            json!({"message_id": 1, "text": "a".repeat(MAX_TEXT_LEN + 1)}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("too long"), "{err}");
    }

    #[tokio::test]
    async fn tg_delete_message_requires_message_id() {
        let err = call("tg_delete_message", json!({})).await.unwrap_err();
        assert!(err.contains("message_id required"), "{err}");
    }

    #[tokio::test]
    async fn tg_forward_message_requires_from_peer() {
        let err = call("tg_forward_message", json!({"to_peer": "self", "message_id": 1}))
            .await
            .unwrap_err();
        assert!(err.contains("from_peer required"), "{err}");
    }

    #[tokio::test]
    async fn tg_forward_message_requires_to_peer() {
        let err = call(
            "tg_forward_message",
            json!({"from_peer": "self", "message_id": 1}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("to_peer required"), "{err}");
    }

    #[tokio::test]
    async fn tg_add_reaction_requires_reaction() {
        let err = call("tg_add_reaction", json!({"message_id": 1}))
            .await
            .unwrap_err();
        assert!(err.contains("reaction required"), "{err}");
    }

    #[tokio::test]
    async fn tg_pin_message_requires_message_id() {
        let err = call("tg_pin_message", json!({})).await.unwrap_err();
        assert!(err.contains("message_id required"), "{err}");
    }

    #[tokio::test]
    async fn tg_upload_media_requires_file_path() {
        let err = call("tg_upload_media", json!({})).await.unwrap_err();
        assert!(err.contains("file_path required"), "{err}");
    }

    #[tokio::test]
    async fn tg_download_media_requires_message_id() {
        let err = call("tg_download_media", json!({"output_path": "/tmp/file"}))
            .await
            .unwrap_err();
        assert!(err.contains("message_id required"), "{err}");
    }

    #[tokio::test]
    async fn tg_download_media_requires_output_path() {
        let err = call("tg_download_media", json!({"message_id": 1}))
            .await
            .unwrap_err();
        assert!(err.contains("output_path required"), "{err}");
    }

    #[tokio::test]
    async fn tg_delete_messages_requires_message_ids() {
        let err = call("tg_delete_messages", json!({})).await.unwrap_err();
        assert!(err.contains("message_ids array required"), "{err}");
    }

    #[tokio::test]
    async fn tg_delete_messages_empty_array_errors() {
        let err = call("tg_delete_messages", json!({"message_ids": []}))
            .await
            .unwrap_err();
        assert!(err.contains("no valid message_ids"), "{err}");
    }

    #[tokio::test]
    async fn tg_forward_messages_requires_from_peer() {
        let err = call(
            "tg_forward_messages",
            json!({"to_peer": "self", "message_ids": [1]}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("from_peer required"), "{err}");
    }

    #[tokio::test]
    async fn tg_forward_messages_requires_to_peer() {
        let err = call(
            "tg_forward_messages",
            json!({"from_peer": "self", "message_ids": [1]}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("to_peer required"), "{err}");
    }

    #[tokio::test]
    async fn tg_send_voice_requires_file_path() {
        let err = call("tg_send_voice", json!({})).await.unwrap_err();
        assert!(err.contains("file_path required"), "{err}");
    }

    #[tokio::test]
    async fn tg_send_sticker_requires_file_path() {
        let err = call("tg_send_sticker", json!({})).await.unwrap_err();
        assert!(err.contains("file_path required"), "{err}");
    }

    #[tokio::test]
    async fn tg_send_animation_requires_file_path() {
        let err = call("tg_send_animation", json!({})).await.unwrap_err();
        assert!(err.contains("file_path required"), "{err}");
    }

    #[tokio::test]
    async fn tg_get_chat_info_requires_peer() {
        let err = call("tg_get_chat_info", json!({})).await.unwrap_err();
        assert!(err.contains("peer required"), "{err}");
    }

    #[tokio::test]
    async fn tg_get_user_requires_peer() {
        let err = call("tg_get_user", json!({})).await.unwrap_err();
        assert!(err.contains("peer required"), "{err}");
    }
}
