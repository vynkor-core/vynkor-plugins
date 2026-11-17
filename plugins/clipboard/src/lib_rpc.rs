use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self { tx }
    }
    pub async fn call(
        &self,
        action: &str,
        params: Value,
        timeout_ms: u32,
    ) -> Result<Value, String> {
        let params_json = serde_json::to_vec(&params)
            .map_err(|e| format!("failed to encode {action} params: {e}"))?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RpcCall {
                action: action.to_string(),
                params_json,
                timeout_ms,
                reply,
            })
            .await
            .map_err(|_| format!("database.{action} aborted: serve loop shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!("database.{action} aborted: shutting down")),
            Err(_) => Err(format!("database.{action} timed out after {effective} ms")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub timeout_ms: u64,
    pub max_bytes: usize,
    pub provider_pref: crate::providers::ProviderPref,
    pub history_enabled: bool,
    pub history_limit: usize,
    pub db_timeout_ms: u32,
}

impl Config {
    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok();
        Self {
            timeout_ms: env(crate::providers::TIMEOUT_MS_ENV)
                .and_then(|v| v.trim().parse().ok())
                .filter(|&ms| ms > 0)
                .unwrap_or(crate::providers::DEFAULT_TIMEOUT_MS),
            max_bytes: env(crate::providers::MAX_BYTES_ENV)
                .and_then(|v| v.trim().parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(crate::providers::DEFAULT_MAX_BYTES),
            provider_pref: crate::providers::parse_provider_pref(
                env(crate::providers::PROVIDER_ENV).as_deref(),
            )
            .unwrap_or(crate::providers::ProviderPref::Auto),
            history_enabled: env("CLIPBOARD_PLUGIN_HISTORY")
                .map(|s| s.trim().to_lowercase())
                .map(|s| s != "0" && s != "false" && s != "off" && s != "no")
                .unwrap_or(true),
            history_limit: env("CLIPBOARD_PLUGIN_HISTORY_LIMIT")
                .and_then(|v| v.trim().parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(crate::history::DEFAULT_HISTORY_LIMIT),
            db_timeout_ms: env("CLIPBOARD_PLUGIN_DB_TIMEOUT_MS")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(5000),
        }
    }
}
