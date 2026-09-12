//! telegram plugin — full MTProto user-client (N-account)
//! Prototype scope: read/write/search + status; media deferred.
//! Architecture: single-reader loop + RPC proxy (see PLUGIN_AUTHORING.md §1).

pub mod mtproto;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// Telegram caps a single message at 4096 characters.
const MAX_TEXT_LEN: usize = 4096;
/// Default page size when `limit` is omitted.
const DEFAULT_LIMIT: u64 = 20;
/// Upper bound for any dialog/history page.
const MAX_LIMIT: u64 = 100;

/// Per-account config derived from env / secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: String, // "personal" | "corporate"
    pub api_id: i32,
    pub api_hash: String,
    pub phone: String,
    pub session_path: String,
}

/// Plugin runtime config (env-driven).
#[derive(Debug, Clone)]
pub struct Config {
    pub accounts: Vec<AccountConfig>,
    pub default_account: String,
    pub session_dir: String,
}

impl Config {
    pub fn from_env() -> Self {
        // TELEGRAM_PLUGIN_ACCOUNTS="personal,corporate"
        // TELEGRAM_PLUGIN_API_ID_personal, _API_HASH_personal, _PHONE_personal
        // fallback to legacy TG_API_ID / TG_API_HASH for single-account compat
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
        }
    }
}

/// Channel-fronted RPC proxy — handlers never touch VynkorClient directly.
#[derive(Clone)]
#[allow(dead_code)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
}

pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<serde_json::Value, String>>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self { tx }
    }
}

/// Result of handling one ActionRequest.
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

/// Dispatch table — implemented in main.rs, mapped here for testing.
///
/// Every handler validates its params first (exact error strings per
/// PLANS.md §5) and returns a stub payload; the real MTProto calls land in
/// [`mtproto`] once the Grammers client is wired up.
pub async fn handle_action(
    _rpc: Rpc,
    config: &Config,
    action: &str,
    params_json: &[u8],
) -> Result<HandleResult, String> {
    let params: Value = serde_json::from_slice(params_json).unwrap_or(Value::Null);
    match action {
        "status" => {
            let v = serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "accounts": config.accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
                "default_account": config.default_account,
            });
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }
        "tg_list_dialogs" => {
            // params: {account?, limit? (default 20, max 100), offset?, filter?}
            let account = resolve_account(&params, config);
            let _limit = clamp_limit(&params);
            // stub — real: grammers Client::get_dialogs with antiban token_bucket
            let v = serde_json::json!({"account": account, "dialogs": [], "total": 0});
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }
        "tg_get_history" => {
            // params: {account?, peer (required), limit? (default 20, max 100), offset_id?}
            // peer == "self" resolves to Saved Messages in the real client
            let peer = required_str(&params, "peer", "tg_get_history")?;
            let _limit = clamp_limit(&params);
            let v = serde_json::json!({"peer": peer, "messages": [], "total": 0});
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }
        "tg_get_message" => {
            // params: {account?, peer, id (required)}
            let peer = optional_peer(&params);
            let id = params
                .get("id")
                .and_then(|v| v.as_u64())
                .filter(|id| *id != 0)
                .ok_or_else(|| "tg_get_message: id required".to_string())?;
            let v = serde_json::json!({"peer": peer, "id": id, "found": false});
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }
        "tg_search" => {
            // params: {account?, query (required, non-empty), peer?, limit?}
            let query = required_str(&params, "query", "tg_search")?;
            let v = serde_json::json!({"query": query, "messages": [], "total": 0});
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: None,
            })
        }
        "tg_send_message" => {
            // params: {account?, peer, text (required, 1..4096), reply_to?}
            let peer = optional_peer(&params);
            let text = required_str(&params, "text", "tg_send_message")?;
            if text.len() > MAX_TEXT_LEN {
                return Err(format!(
                    "tg_send_message: text too long (max {MAX_TEXT_LEN})"
                ));
            }
            // real: antiban check + FloodWait + send via grammers
            let v = serde_json::json!({"peer": peer, "message_id": 1, "text": text});
            let ev = EventToPublish {
                event_type: "plugin.telegram.message_sent".into(),
                payload: serde_json::json!({"peer": peer, "message_id": 1}),
            };
            Ok(HandleResult {
                data: serde_json::to_vec(&v).unwrap(),
                event: Some(ev),
            })
        }
        other => Err(format!("unknown action: {other}")),
    }
}

/// `account` param if present and non-empty, else the configured default.
fn resolve_account<'a>(params: &'a Value, config: &'a Config) -> &'a str {
    params
        .get("account")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&config.default_account)
}

/// `limit` param defaulting to 20, clamped to 1..=100.
fn clamp_limit(params: &Value) -> u64 {
    params
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT)
}

/// `peer` param, defaulting to `"self"` (Saved Messages).
fn optional_peer(params: &Value) -> &str {
    params
        .get("peer")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("self")
}

/// A required non-empty string param; missing/blank surfaces an error naming
/// the action and key (e.g. `tg_search: query required`).
fn required_str<'a>(params: &'a Value, key: &str, action: &str) -> Result<&'a str, String> {
    match params.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(format!("{action}: {key} required")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;

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
        }
    }

    fn rpc() -> Rpc {
        Rpc::new(mpsc::channel(1).0)
    }

    async fn call(action: &str, params: Value) -> Result<HandleResult, String> {
        let cfg = test_config();
        let bytes = serde_json::to_vec(&params).unwrap();
        handle_action(rpc(), &cfg, action, &bytes).await
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
    async fn tg_send_message_ok_publishes_event() {
        let res = call("tg_send_message", json!({"peer": "self", "text": "hello"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&res.data).unwrap();
        assert_eq!(v["peer"], "self");
        assert_eq!(v["message_id"], 1);
        assert_eq!(v["text"], "hello");

        let ev = res.event.expect("send should publish a message_sent event");
        assert_eq!(ev.event_type, "plugin.telegram.message_sent");
        assert_eq!(ev.payload["peer"], "self");
        assert_eq!(ev.payload["message_id"], 1);
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
    }

    #[tokio::test]
    async fn tg_list_dialogs_returns_empty_for_stub() {
        let res = call("tg_list_dialogs", json!({})).await.unwrap();
        let v: Value = serde_json::from_slice(&res.data).unwrap();
        assert_eq!(v["account"], "personal");
        assert_eq!(v["total"], 0);
    }

    #[tokio::test]
    async fn tg_get_history_with_peer_returns_empty() {
        let res = call("tg_get_history", json!({"peer": "test"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&res.data).unwrap();
        assert_eq!(v["peer"], "test");
        assert_eq!(v["total"], 0);
    }

    #[tokio::test]
    async fn tg_search_empty_query_errors() {
        let err = call("tg_search", json!({"query": ""})).await.unwrap_err();
        assert!(err.contains("query required"), "{err}");
    }

    #[tokio::test]
    async fn tg_send_message_default_peer_is_self() {
        let res = call("tg_send_message", json!({"text": "hi"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&res.data).unwrap();
        assert_eq!(v["peer"], "self");
    }

    #[tokio::test]
    async fn unknown_action_returns_error() {
        let err = call("bogus_action", json!({})).await.unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }

    #[tokio::test]
    async fn tg_get_message_missing_peer_defaults_to_self() {
        let res = call("tg_get_message", json!({"id": 1})).await.unwrap();
        let v: Value = serde_json::from_slice(&res.data).unwrap();
        assert_eq!(v["peer"], "self");
        assert_eq!(v["found"], false);
    }
}
