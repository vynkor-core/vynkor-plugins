//! telegram plugin — full MTProto user-client (N-account)
//! Prototype scope: read/write/search + status; media deferred.
//! Architecture: single-reader loop + RPC proxy (see PLUGIN_AUTHORING.md §1).

pub mod mtproto;

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use mtproto::SessionPool;

const MAX_TEXT_LEN: usize = 4096;
const DEFAULT_LIMIT: u64 = 20;
const MAX_LIMIT: u64 = 100;

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
        }
    }

    pub async fn connect_all(&mut self) -> Result<(), String> {
        if self.accounts.is_empty() {
            return Ok(());
        }
        let pool = Arc::new(SessionPool::new());
        for account in &self.accounts {
            pool.connect(account)
                .await
                .map_err(|e| format!("failed to connect {}: {e}", account.id))?;
        }
        self.pool = Some(pool);
        Ok(())
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
    let limit = clamp_limit(params);
    let client = get_client(config, account)?;

    let mut dialogs = client.iter_dialogs();
    let mut result = Vec::new();
    let mut total = 0u64;

    while let Some(dialog) = dialogs.next().await.map_err(|e| format!("dialog iter: {e}"))? {
        total += 1;
        if result.len() >= limit as usize {
            continue;
        }
        let peer = dialog.peer();
        result.push(serde_json::json!({
            "peer": peer_to_string(peer),
        }));
    }

    let v = serde_json::json!({"account": account, "dialogs": result, "total": total});
    Ok(HandleResult {
        data: serde_json::to_vec(&v).unwrap(),
        event: None,
    })
}

async fn handle_get_history(config: &Config, params: &Value) -> Result<HandleResult, String> {
    let peer_str = required_str(params, "peer", "tg_get_history")?;
    let limit = clamp_limit(params);
    let account = resolve_account(params, config);
    let client = get_client(config, account)?;
    let peer = parse_peer(peer_str)?;

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
    let client = get_client(config, account)?;
    let peer = parse_peer(optional_peer(params))?;

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
    let client = get_client(config, account)?;
    let peer = parse_peer(peer_str)?;

    let reply_to = params.get("reply_to").and_then(|v| v.as_u64()).map(|id| id as i32);

    let mut msg = grammers_client::types::InputMessage::default().text(text);
    if let Some(reply_id) = reply_to {
        msg = msg.reply_to(Some(reply_id));
    }

    let sent = client.send_message(peer, msg).await
        .map_err(|e| format!("send_message failed: {e}"))?;

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

fn get_client(config: &Config, account: &str) -> Result<grammers_client::Client, String> {
    config
        .pool
        .as_ref()
        .ok_or_else(|| "no telegram accounts connected".to_string())?
        .get(account)
        .ok_or_else(|| format!("account not connected: {account}"))
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
        return Err("username resolution not supported, use id format".into());
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
}
