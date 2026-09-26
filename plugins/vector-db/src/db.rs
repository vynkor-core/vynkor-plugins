use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use tokio::sync::Mutex;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct DbConfig {
    pub data_dir: PathBuf,
    pub pool_size: u32,
    pub busy_timeout_ms: u64,
    pub max_db_bytes: u64,
}

pub struct DbPools {
    config: DbConfig,
    pools: Mutex<HashMap<String, SqlitePool>>,
}

const PAGE_SIZE: u32 = 4096;

const COLLECTIONS_DDL: &str = "create table if not exists collections (\
    name TEXT PRIMARY KEY, \
    dim INTEGER NOT NULL, \
    created_at INTEGER NOT NULL\
)";

const VECTORS_DDL: &str = "create table if not exists vectors (\
    collection TEXT NOT NULL, \
    id TEXT NOT NULL, \
    text TEXT NOT NULL DEFAULT '', \
    vector TEXT NOT NULL, \
    metadata TEXT NOT NULL DEFAULT '{}', \
    updated_at INTEGER NOT NULL, \
    PRIMARY KEY (collection, id)\
)";

const VECTORS_INDEX_DDL: &str =
    "create index if not exists idx_vectors_collection on vectors(collection)";

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

pub fn sanitize_collection(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        return Err("collection must be non-empty".to_string());
    }
    if name.len() > 128 {
        return Err("collection name too long (max 128)".to_string());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
    {
        return Err(format!("invalid collection name {name:?}: allowed [a-zA-Z0-9_.-]"));
    }
    Ok(name)
}

pub fn sanitize_id(id: &str) -> Result<&str, String> {
    if id.is_empty() {
        return Err("id must be non-empty".to_string());
    }
    if id.len() > 256 {
        return Err("id too long (max 256)".to_string());
    }
    if id.contains('\0') {
        return Err("id must not contain null bytes".to_string());
    }
    Ok(id)
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

        // Held across creation on purpose: concurrent first calls for one
        // caller used to each open their own pool on the same fresh file and
        // race the WAL switch into SQLITE_BUSY ("database is locked"), which
        // busy_timeout does not cover. Creation happens once per caller.
        let mut pools = self.pools.lock().await;
        if let Some(pool) = pools.get(caller_id) {
            return Ok(pool.clone());
        }

        std::fs::create_dir_all(&self.config.data_dir)
            .map_err(|e| format!("failed to create data_dir: {e}"))?;
        let db_path = self.config.data_dir.join(format!("{caller_id}.db"));

        let mut options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
            .map_err(|e| format!("invalid sqlite path: {e}"))?
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_millis(self.config.busy_timeout_ms))
            .page_size(PAGE_SIZE)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

        if self.config.max_db_bytes > 0 {
            let max_pages = (self.config.max_db_bytes / PAGE_SIZE as u64).max(1);
            options = options.pragma("max_page_count", max_pages.to_string());
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(self.config.pool_size)
            .connect_with(options)
            .await
            .map_err(|e| format!("failed to open database for {caller_id}: {e}"))?;

        sqlx::query(COLLECTIONS_DDL)
            .execute(&pool)
            .await
            .map_err(|e| format!("failed to init collections table for {caller_id}: {e}"))?;
        sqlx::query(VECTORS_DDL)
            .execute(&pool)
            .await
            .map_err(|e| format!("failed to init vectors table for {caller_id}: {e}"))?;
        sqlx::query(VECTORS_INDEX_DDL)
            .execute(&pool)
            .await
            .map_err(|e| format!("failed to init vectors index for {caller_id}: {e}"))?;

        pools.insert(caller_id.to_string(), pool.clone());
        Ok(pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_accepts_normal_id() {
        assert_eq!(sanitize_caller_id("notes_plugin-v2").unwrap(), "notes_plugin-v2");
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
    fn sanitize_collection_ok() {
        assert!(sanitize_collection("agent_memory").is_ok());
        assert!(sanitize_collection("my.collection-1").is_ok());
    }

    #[test]
    fn sanitize_collection_rejects_bad() {
        assert!(sanitize_collection("").is_err());
        assert!(sanitize_collection("has space").is_err());
        assert!(sanitize_collection("has/slash").is_err());
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
        sqlx::query("insert into collections (name, dim, created_at) values ('c', 3, 0)")
            .execute(&pool_a)
            .await
            .unwrap();

        let pool_a_again = pools.pool_for("caller_a").await.unwrap();
        let row: (i64,) = sqlx::query_as("select count(*) from collections")
            .fetch_one(&pool_a_again)
            .await
            .unwrap();
        assert_eq!(row.0, 1);

        let pool_b = pools.pool_for("caller_b").await.unwrap();
        let row_b: (i64,) = sqlx::query_as("select count(*) from collections")
            .fetch_one(&pool_b)
            .await
            .unwrap();
        assert_eq!(row_b.0, 0);
    }
}
