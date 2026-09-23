use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use grammers_client::Client;
use grammers_client::types::Dialog;
use grammers_mtsender::{ConnectionParams, SenderPool, SenderPoolHandle};
use grammers_session::storages::SqliteSession;
use grammers_session::updates::UpdatesLike;
use tokio::sync::mpsc;

use crate::AccountConfig;
use crate::mtproto::Antiban;

const DIALOG_CACHE_TTL: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);

struct DialogCacheEntry {
    fetched_at: Instant,
    dialogs: Vec<Dialog>,
}

pub struct SessionPool {
    clients: RwLock<HashMap<String, Client>>,
    _handles: RwLock<HashMap<String, SenderPoolHandle>>,
    pub antiban: Arc<Antiban>,
    dialog_cache: RwLock<HashMap<String, DialogCacheEntry>>,
}

impl std::fmt::Debug for SessionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let clients = self.clients.read().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("SessionPool")
            .field("accounts", &clients.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SessionPool {
    pub fn new() -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
            _handles: RwLock::new(HashMap::new()),
            antiban: Arc::new(Antiban::new()),
            dialog_cache: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_dialogs_cached(
        &self,
        account: &str,
        client: &Client,
    ) -> Result<Vec<Dialog>, String> {
        {
            let cache = self
                .dialog_cache
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.get(account) {
                if entry.fetched_at.elapsed() < DIALOG_CACHE_TTL {
                    return Ok(entry.dialogs.clone());
                }
            }
        }
        let mut iter = client.iter_dialogs();
        let mut dialogs = Vec::new();
        while let Some(d) = iter
            .next()
            .await
            .map_err(|e| format!("dialog iter: {e}"))?
        {
            dialogs.push(d);
        }
        let mut cache = self
            .dialog_cache
            .write()
            .unwrap_or_else(|e| e.into_inner());
        cache.insert(
            account.to_string(),
            DialogCacheEntry {
                fetched_at: Instant::now(),
                dialogs: dialogs.clone(),
            },
        );
        Ok(dialogs)
    }

    pub fn invalidate_dialogs(&self, account: &str) {
        if let Ok(mut cache) = self.dialog_cache.write() {
            cache.remove(account);
        }
    }

    pub async fn connect(
        &self,
        account: &AccountConfig,
    ) -> Result<mpsc::UnboundedReceiver<UpdatesLike>> {
        let session = std::sync::Arc::new(
            SqliteSession::open(&account.session_path).with_context(|| {
                format!(
                    "failed to load/create session file {}",
                    account.session_path
                )
            })?,
        );
        let proxy_url = std::env::var("TELEGRAM_PLUGIN_PROXY_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let params = ConnectionParams {
            proxy_url: proxy_url.clone(),
            ..Default::default()
        };
        let pool = SenderPool::with_configuration(session, account.api_id, params);
        let client = Client::new(&pool);
        let handle = pool.handle.clone();
        let updates_rx = pool.updates;

        let runner = tokio::spawn(pool.runner.run());

        // A stale/invalid session file loads without error but isn't
        // actually logged in — every subsequent action would then fail with
        // an opaque grammers auth error while `status` still reports
        // `engine_ready: true`. Fail fast here with a clear message instead.
        //
        // Bounded by a timeout: when the account's home DC is unreachable
        // (network-level block), grammers' sender keeps reconnecting forever
        // and eventually overflows a worker stack, aborting the process with
        // no useful message. Give up cleanly and stop the runner instead.
        let auth = tokio::time::timeout(CONNECT_TIMEOUT, client.is_authorized()).await;
        let err = match auth {
            Ok(Ok(true)) => None,
            Ok(Ok(false)) => Some(format!(
                "account {}: session not authorized — re-run the login flow",
                account.id
            )),
            Ok(Err(e)) => Some(format!(
                "account {}: failed to verify session auth state: {e}",
                account.id
            )),
            Err(_) => Some(format!(
                "account {}: could not reach Telegram within {}s (proxy: {}) — home DC likely blocked on this network; set TELEGRAM_PLUGIN_PROXY_URL=socks5://host:port",
                account.id,
                CONNECT_TIMEOUT.as_secs(),
                if proxy_url.is_some() { "on" } else { "off" },
            )),
        };
        if let Some(msg) = err {
            runner.abort();
            bail!(msg);
        }

        self._handles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.id.clone(), handle);
        self.clients
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.id.clone(), client);

        Ok(updates_rx)
    }

    pub fn get(&self, account: &str) -> Option<Client> {
        self.clients
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account)
            .cloned()
    }

    pub fn disconnect_all(&self) {
        self._handles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.clients
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

impl Default for SessionPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pool_returns_none() {
        let pool = SessionPool::new();
        assert!(pool.get("personal").is_none());
    }

    #[test]
    fn disconnect_all_is_idempotent() {
        let pool = SessionPool::new();
        pool.disconnect_all();
        pool.disconnect_all();
        assert!(pool.get("personal").is_none());
    }
}
