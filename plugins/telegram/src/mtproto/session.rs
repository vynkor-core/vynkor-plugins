//! Per-account Grammers client pool.
//!
//! One [`grammers_client::Client`] per Telegram account, keyed by account id.
//! Connecting loads (or creates) the account's session file and opens an
//! MTProto connection; callers then grab the client handle by account id.

use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::{Context, Result};
use grammers_client::Client;
use grammers_mtsender::{SenderPool, SenderPoolHandle};
use grammers_session::storages::SqliteSession;

use crate::AccountConfig;

/// Pool of connected Grammers clients, one per Telegram account.
///
/// [`Client`] is a cheap `Arc` clone, so [`SessionPool::get`] hands back an
/// owned handle rather than borrowing through the internal lock — the borrow
/// could not outlive the read guard.
pub struct SessionPool {
    clients: RwLock<HashMap<String, Client>>,
    _handles: RwLock<HashMap<String, SenderPoolHandle>>,
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
        }
    }

    /// Load (or create) `account.session_path`, then open a Grammers client
    /// and store it keyed by `account.id`. The session file persists on disk
    /// so a reconnect after the plugin restarts reuses the authorization key.
    pub async fn connect(&self, account: &AccountConfig) -> Result<()> {
        let session = std::sync::Arc::new(
            SqliteSession::open(&account.session_path).with_context(|| {
                format!(
                    "failed to load/create session file {}",
                    account.session_path
                )
            })?,
        );
        let pool = SenderPool::new(session, account.api_id);
        let client = Client::new(&pool);
        let handle = pool.handle.clone();

        // Spawn the network runner in the background
        tokio::spawn(pool.runner.run());

        self._handles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.id.clone(), handle);
        self.clients
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.id.clone(), client);
        Ok(())
    }

    /// Look up a connected account's client by id.
    pub fn get(&self, account: &str) -> Option<Client> {
        self.clients
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account)
            .cloned()
    }

    /// Drop every connected client, closing all MTProto connections.
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
