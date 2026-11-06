//! Shared handle to the single SQLite connection.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;

/// One writer: every access goes through this mutex, which is what keeps the
/// per-transaction revision counter strictly monotonic.
pub type Db = Arc<Mutex<Connection>>;

/// Run `f` on the blocking pool under the connection lock.
pub async fn with_db<T, F>(db: &Db, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> T + Send + 'static,
{
    let db = Arc::clone(db);
    tokio::task::spawn_blocking(move || {
        let mut guard = db.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    })
    .await
    .map_err(|e| format!("internal error: {e}"))
}
