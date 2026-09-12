use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::{Context, Result};
use grammers_client::Client;
use grammers_mtsender::{SenderPool, SenderPoolHandle};
use grammers_session::storages::SqliteSession;
use grammers_session::updates::UpdatesLike;
use tokio::sync::mpsc;

use crate::AccountConfig;
use crate::mtproto::Antiban;

pub struct SessionPool {
    clients: RwLock<HashMap<String, Client>>,
    _handles: RwLock<HashMap<String, SenderPoolHandle>>,
    pub antiban: Arc<Antiban>,
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
        let pool = SenderPool::new(session, account.api_id);
        let client = Client::new(&pool);
        let handle = pool.handle.clone();
        let updates_rx = pool.updates;

        tokio::spawn(pool.runner.run());

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
