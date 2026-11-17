use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use tokio::sync::Mutex;

/// Current unix time in milliseconds (the same stamp `updated_at` uses).
/// Saturates at `i64::MAX` rather than panicking on a clock before the
/// epoch; a saturated stamp is harmless for both `updated_at` and the
/// expiry sweep (it can only make a key look newer/alive, never expire it).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub struct DbConfig {
    pub data_dir: PathBuf,
    pub pool_size: u32,
    pub busy_timeout_ms: u64,
    /// Hard per-caller storage ceiling, enforced by SQLite itself via
    /// `PRAGMA max_page_count` (writes past it fail with `SQLITE_FULL`).
    /// This is the real cap on raw-SQL writes — `max_value_bytes` only
    /// guards the `db_set` fast path and is trivially bypassable with a raw
    /// `INSERT`. `0` disables the quota.
    pub max_db_bytes: u64,
}

pub struct DbPools {
    config: DbConfig,
    pools: Mutex<HashMap<String, SqlitePool>>,
}

/// Fixed SQLite page size, set explicitly so the byte→page arithmetic for
/// the `max_page_count` quota (see `pool_for`) is exact rather than
/// depending on the compiled-in default.
const PAGE_SIZE: u32 = 4096;

const KV_TABLE_DDL: &str = "create table if not exists kv (\
    key TEXT PRIMARY KEY, \
    value TEXT NOT NULL, \
    updated_at INTEGER NOT NULL, \
    expires_at INTEGER\
)";

/// Caller ids are kernel-stamped plugin ids, never caller-controlled path
/// data in practice — this check is defense in depth (design spec: "the
/// kernel registry is the real gate on plugin ids"), rejecting anything
/// that isn't a plain filename-safe token so a compromised/buggy kernel
/// can't turn this into a path traversal.
pub fn sanitize_caller_id(caller_plugin_id: &str) -> Result<&str, String> {
    if caller_plugin_id.is_empty() {
        return Err("missing caller_plugin_id".to_string());
    }
    if !caller_plugin_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!("invalid caller_plugin_id: {caller_plugin_id:?}"));
    }
    Ok(caller_plugin_id)
}

/// Keyword pre-check fallback for blocking `ATTACH` (see plan's Global
/// Constraints for why this instead of SQLITE_LIMIT_ATTACHED). Matches
/// "attach" as a whole word, case-insensitive, anywhere in the statement —
/// deliberately over-broad (rejects `SELECT 'attach'` too) since the only
/// cost of a false positive is a caller rewording their SQL, while a false
/// negative would be a real isolation bypass.
///
/// A quote character immediately before/after "attach" IS a valid word
/// boundary (quotes are never part of an identifier or keyword) — so
/// `ATTACH'evil.db' AS x` (no space between the keyword and the quote,
/// valid SQLite syntax) still counts as a whole-word match. The one
/// exception: if "attach" is preceded by a quote AND followed by that same
/// quote character (e.g. `'attach'`), then "attach" is the entire contents
/// of a quoted string literal, not a bare keyword, so that case is not
/// flagged.
pub fn rejects_attach(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let needle = b"attach";
    let mut i = 0;
    while let Some(pos) = find_from(bytes, needle, i) {
        let before_ok =
            pos == 0 || (!bytes[pos - 1].is_ascii_alphanumeric() && bytes[pos - 1] != b'_');
        let after = pos + needle.len();
        let after_ok =
            after == bytes.len() || (!bytes[after].is_ascii_alphanumeric() && bytes[after] != b'_');

        let quote_before = (pos > 0 && (bytes[pos - 1] == b'\'' || bytes[pos - 1] == b'"'))
            .then(|| bytes[pos - 1]);
        let quote_after = (after < bytes.len() && (bytes[after] == b'\'' || bytes[after] == b'"'))
            .then(|| bytes[after]);
        let is_quoted_literal_contents =
            matches!((quote_before, quote_after), (Some(a), Some(b)) if a == b);

        if before_ok && after_ok && !is_quoted_literal_contents {
            return true;
        }
        i = pos + 1;
    }
    false
}

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

impl DbPools {
    pub fn new(config: DbConfig) -> Self {
        Self {
            config,
            pools: Mutex::new(HashMap::new()),
        }
    }

    pub async fn pool_for(&self, caller_plugin_id: &str) -> Result<SqlitePool, String> {
        let caller_id = sanitize_caller_id(caller_plugin_id)?;

        // Fast path: only hold the lock long enough to check the cache, so a
        // slow first connect for one caller can't block every other
        // caller's pool_for call.
        {
            let pools = self.pools.lock().await;
            if let Some(pool) = pools.get(caller_id) {
                return Ok(pool.clone());
            }
        }

        std::fs::create_dir_all(&self.config.data_dir)
            .map_err(|e| format!("failed to create data_dir: {e}"))?;
        let db_path = self.config.data_dir.join(format!("{caller_id}.db"));

        let mut options =
            SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
                .map_err(|e| format!("invalid sqlite path: {e}"))?
                .create_if_missing(true)
                .busy_timeout(std::time::Duration::from_millis(
                    self.config.busy_timeout_ms,
                ))
                .page_size(PAGE_SIZE)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

        // Hard per-caller storage cap (bug #1). `PRAGMA max_page_count` is
        // enforced by SQLite itself — a write that would grow the file past
        // the limit fails with SQLITE_FULL — so it bounds raw `db_query`
        // INSERTs that sidestep the `db_set`-only `max_value_bytes` check.
        // sqlx replays these pragmas on every pooled connection (see
        // SqliteConnectOptions::pragma_string), so the ceiling holds across
        // the whole pool, not just the first connection. `0` disables it.
        if self.config.max_db_bytes > 0 {
            let max_pages = (self.config.max_db_bytes / PAGE_SIZE as u64).max(1);
            options = options.pragma("max_page_count", max_pages.to_string());
        }

        // Unlocked: real file I/O + connection setup. Two callers racing on
        // the same caller_id will both open a connection to the same file
        // and both run the idempotent `CREATE TABLE IF NOT EXISTS` DDL —
        // WAL mode allows concurrent connections to the same SQLite file,
        // so this can't produce two db files or a conflicting table-init.
        let pool = SqlitePoolOptions::new()
            .max_connections(self.config.pool_size)
            .connect_with(options)
            .await
            .map_err(|e| format!("failed to open database for {caller_id}: {e}"))?;

        sqlx::query(KV_TABLE_DDL)
            .execute(&pool)
            .await
            .map_err(|e| format!("failed to init kv table for {caller_id}: {e}"))?;

        // Migration for `.db` files created before v0.3 (which predate the
        // `expires_at` column): add the column when it's missing. Idempotent —
        // the pragma check makes this a no-op on files already migrated or
        // freshly created by the DDL above.
        let has_expires: (i64,) = sqlx::query_as(
            "select count(*) from pragma_table_info('kv') where name = 'expires_at'",
        )
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("failed to check kv schema for {caller_id}: {e}"))?;
        if has_expires.0 == 0 {
            sqlx::query("alter table kv add column expires_at INTEGER")
                .execute(&pool)
                .await
                .map_err(|e| format!("failed to migrate kv schema for {caller_id}: {e}"))?;
        }

        // Re-lock only to insert. If another caller raced us and already
        // cached a pool for this caller_id, keep the winner's pool (both
        // are equally valid — same file, same DDL already applied) and let
        // our redundant one be dropped, so exactly one pool per caller_id
        // ends up cached.
        let mut pools = self.pools.lock().await;
        let winner = pools
            .entry(caller_id.to_string())
            .or_insert_with(|| pool.clone())
            .clone();
        Ok(winner)
    }
}

/// Delete every kv row whose `expires_at` has passed (`<= now` — a row
/// expiring exactly at `now` is already dead). Called by the handler before
/// every action, so expired keys are gone before any accessor runs.
pub async fn sweep_expired(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query("delete from kv where expires_at is not null and expires_at <= ?1")
        .bind(now_ms())
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_accepts_normal_id() {
        assert_eq!(
            sanitize_caller_id("notes_plugin-v2").unwrap(),
            "notes_plugin-v2"
        );
    }

    #[test]
    fn sanitize_rejects_empty() {
        assert!(sanitize_caller_id("").is_err());
    }

    #[test]
    fn sanitize_rejects_path_traversal() {
        assert!(sanitize_caller_id("../etc/passwd").is_err());
        assert!(sanitize_caller_id("foo/bar").is_err());
        assert!(sanitize_caller_id("foo.db").is_err());
    }

    #[test]
    fn attach_detection_catches_various_forms() {
        assert!(rejects_attach("ATTACH DATABASE 'x' AS y"));
        assert!(rejects_attach("attach database 'x' as y"));
        assert!(rejects_attach("select 1; attach database 'x' as y"));
        // No whitespace between the keyword and the quote — still valid
        // SQLite syntax, and previously bypassed detection.
        assert!(rejects_attach("ATTACH'evil.db' AS x"));
    }

    #[test]
    fn attach_detection_ignores_substrings() {
        assert!(!rejects_attach("select * from attachments"));
        assert!(!rejects_attach("select 'attach' as label"));
        // Bare quoted word: "attach" is the entire contents of a string
        // literal, not a statement keyword.
        assert!(!rejects_attach("select 'attach'"));
    }

    #[tokio::test]
    async fn pool_for_creates_and_reuses_per_caller_file() {
        let dir = tempfile::tempdir().unwrap();
        let pools = DbPools::new(DbConfig {
            data_dir: dir.path().to_path_buf(),
            pool_size: 2,
            busy_timeout_ms: 1000,
            max_db_bytes: 0,
        });

        let pool_a = pools.pool_for("caller_a").await.unwrap();
        sqlx::query("insert into kv (key, value, updated_at) values ('k', 'v', 0)")
            .execute(&pool_a)
            .await
            .unwrap();

        // Second call for the same caller reuses the pool/file (row persists).
        let pool_a_again = pools.pool_for("caller_a").await.unwrap();
        let row: (i64,) = sqlx::query_as("select count(*) from kv")
            .fetch_one(&pool_a_again)
            .await
            .unwrap();
        assert_eq!(row.0, 1);

        // A different caller gets a separate, empty file.
        let pool_b = pools.pool_for("caller_b").await.unwrap();
        let row_b: (i64,) = sqlx::query_as("select count(*) from kv")
            .fetch_one(&pool_b)
            .await
            .unwrap();
        assert_eq!(row_b.0, 0);

        assert!(dir.path().join("caller_a.db").exists());
        assert!(dir.path().join("caller_b.db").exists());
    }

    #[tokio::test]
    async fn pool_for_rejects_bad_caller_id() {
        let dir = tempfile::tempdir().unwrap();
        let pools = DbPools::new(DbConfig {
            data_dir: dir.path().to_path_buf(),
            pool_size: 2,
            busy_timeout_ms: 1000,
            max_db_bytes: 0,
        });
        assert!(pools.pool_for("../escape").await.is_err());
        assert!(pools.pool_for("").await.is_err());
    }
}
