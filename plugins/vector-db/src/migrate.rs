//! Schema migrations driven by `PRAGMA user_version`.
//!
//! v0 (legacy): `vectors.vector` is a JSON text array, `collections` has no `model`.
//! v1: `vectors.vector` is an f32 little-endian BLOB, `collections.model TEXT`.

use std::path::Path;

use sqlx::{Row, SqlitePool};

use crate::codec;

pub const SCHEMA_VERSION: i64 = 1;

pub const COLLECTIONS_DDL: &str = "create table if not exists collections (\
    name TEXT PRIMARY KEY, \
    dim INTEGER NOT NULL, \
    model TEXT NOT NULL DEFAULT '', \
    created_at INTEGER NOT NULL\
)";

pub const VECTORS_DDL: &str = "create table if not exists vectors (\
    collection TEXT NOT NULL, \
    id TEXT NOT NULL, \
    text TEXT NOT NULL DEFAULT '', \
    vector BLOB NOT NULL, \
    metadata TEXT NOT NULL DEFAULT '{}', \
    updated_at INTEGER NOT NULL, \
    PRIMARY KEY (collection, id)\
)";

pub const VECTORS_INDEX_DDL: &str =
    "create index if not exists idx_vectors_collection on vectors(collection)";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrateReport {
    /// user_version found on open.
    pub from_version: i64,
    pub migrated_rows: usize,
    /// Rows with an unparsable JSON vector that were dropped (ids logged).
    pub skipped_rows: usize,
    /// Backup written by this run (None when nothing to back up or already present).
    pub backup: Option<std::path::PathBuf>,
}

pub fn backup_path(db_path: &Path) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".bak-v0");
    s.into()
}

pub async fn run(pool: &SqlitePool, db_path: &Path) -> Result<MigrateReport, String> {
    let version: i64 = sqlx::query_scalar("pragma user_version")
        .fetch_one(pool)
        .await
        .map_err(|e| format!("failed to read user_version: {e}"))?;
    let mut report = MigrateReport {
        from_version: version,
        ..Default::default()
    };
    if version >= SCHEMA_VERSION {
        return Ok(report);
    }

    let has_vectors: i64 = sqlx::query_scalar(
        "select count(*) from sqlite_master where type = 'table' and name = 'vectors'",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;

    if has_vectors == 0 {
        // Fresh database: create the current schema directly.
        for ddl in [COLLECTIONS_DDL, VECTORS_DDL, VECTORS_INDEX_DDL] {
            sqlx::query(ddl)
                .execute(pool)
                .await
                .map_err(|e| format!("failed to init schema: {e}"))?;
        }
        sqlx::query(&format!("pragma user_version = {SCHEMA_VERSION}"))
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(report);
    }

    // Legacy v0 database with live data: back it up first, once.
    let bak = backup_path(db_path);
    if !bak.exists() {
        sqlx::query("vacuum into ?1")
            .bind(bak.to_string_lossy().to_string())
            .execute(pool)
            .await
            .map_err(|e| format!("failed to back up database to {}: {e}", bak.display()))?;
        eprintln!(
            "[vector-db] migrating {} to v1; backup at {}",
            db_path.display(),
            bak.display()
        );
        report.backup = Some(bak);
    }

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(
        "create table vectors_v1 (\
        collection TEXT NOT NULL, id TEXT NOT NULL, text TEXT NOT NULL DEFAULT '', \
        vector BLOB NOT NULL, metadata TEXT NOT NULL DEFAULT '{}', updated_at INTEGER NOT NULL, \
        PRIMARY KEY (collection, id))",
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    let mut last_rowid: i64 = 0;
    loop {
        let rows = sqlx::query(
            "select rowid, collection, id, text, vector, metadata, updated_at from vectors \
             where rowid > ?1 order by rowid limit 1000",
        )
        .bind(last_rowid)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            last_rowid = row.get::<i64, _>("rowid");
            let collection: String = row.get("collection");
            let id: String = row.get("id");
            let json: String = row.get("vector");
            let parsed: Result<Vec<f32>, _> = serde_json::from_str(&json);
            let vec = match parsed {
                Ok(v) if !v.is_empty() => v,
                other => {
                    let why = other
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "empty".into());
                    eprintln!(
                        "[vector-db] warn: skipping corrupt vector during migration: collection={collection} id={id}: {why}"
                    );
                    report.skipped_rows += 1;
                    continue;
                }
            };
            sqlx::query(
                "insert into vectors_v1 (collection, id, text, vector, metadata, updated_at) \
                 values (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .bind(collection)
            .bind(id)
            .bind(row.get::<String, _>("text"))
            .bind(codec::encode(&vec))
            .bind(row.get::<String, _>("metadata"))
            .bind(row.get::<i64, _>("updated_at"))
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
            report.migrated_rows += 1;
        }
    }

    sqlx::query("drop table vectors")
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("alter table vectors_v1 rename to vectors")
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(VECTORS_INDEX_DDL)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    let has_model: i64 = sqlx::query_scalar(
        "select count(*) from pragma_table_info('collections') where name = 'model'",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    if has_model == 0 {
        sqlx::query("alter table collections add column model TEXT NOT NULL DEFAULT ''")
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    sqlx::query(&format!("pragma user_version = {SCHEMA_VERSION}"))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit()
        .await
        .map_err(|e| format!("migration commit failed: {e}"))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{DbConfig, DbPools};
    use crate::handler::Handler;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;
    use std::str::FromStr;

    const V0_COLLECTIONS: &str = "create table if not exists collections (\
        name TEXT PRIMARY KEY, dim INTEGER NOT NULL, created_at INTEGER NOT NULL)";
    const V0_VECTORS: &str = "create table if not exists vectors (\
        collection TEXT NOT NULL, id TEXT NOT NULL, text TEXT NOT NULL DEFAULT '', \
        vector TEXT NOT NULL, metadata TEXT NOT NULL DEFAULT '{}', updated_at INTEGER NOT NULL, \
        PRIMARY KEY (collection, id))";

    fn norm(v: &[f32]) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    /// Build a legacy v0 database file with JSON-text vectors.
    async fn make_v0(dir: &Path, caller: &str, rows: &[(&str, &str, &str)]) {
        let path = dir.join(format!("{caller}.db"));
        let mut conn = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        sqlx::query(V0_COLLECTIONS)
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query(V0_VECTORS).execute(&mut conn).await.unwrap();
        sqlx::query("insert into collections (name, dim, created_at) values ('mem', 4, 1)")
            .execute(&mut conn)
            .await
            .unwrap();
        for (id, text, vec_json) in rows {
            sqlx::query(
                "insert into vectors (collection, id, text, vector, metadata, updated_at) \
                 values ('mem', ?1, ?2, ?3, '{\"k\":\"v\"}', 5)",
            )
            .bind(id)
            .bind(text)
            .bind(vec_json)
            .execute(&mut conn)
            .await
            .unwrap();
        }
    }

    fn pools(dir: &Path) -> DbPools {
        DbPools::new(DbConfig {
            data_dir: dir.to_path_buf(),
            pool_size: 2,
            busy_timeout_ms: 2000,
            max_db_bytes: 0,
        })
    }

    fn sample_rows() -> Vec<(&'static str, &'static str, String)> {
        let vs = [
            ("a", "alpha", norm(&[1.0, 0.2, 0.0, 0.1])),
            ("b", "beta", norm(&[0.0, 1.0, 0.3, 0.0])),
            ("c", "gamma", norm(&[0.3, 0.3, 1.0, 0.7])),
        ];
        vs.iter()
            .map(|(i, t, v)| (*i, *t, serde_json::to_string(v).unwrap()))
            .collect()
    }

    #[tokio::test]
    async fn migrates_json_vectors_to_blob_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        let rows = sample_rows();
        let refs: Vec<(&str, &str, &str)> =
            rows.iter().map(|(a, b, c)| (*a, *b, c.as_str())).collect();
        make_v0(dir.path(), "agent", &refs).await;

        let p = pools(dir.path());
        let pool = p.pool_for("agent").await.unwrap();
        let v: i64 = sqlx::query_scalar("pragma user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, 1);
        assert!(backup_path(&dir.path().join("agent.db")).exists());
        let ty: String = sqlx::query_scalar("select typeof(vector) from vectors where id='a'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(ty, "blob");
        let model_cols: i64 = sqlx::query_scalar(
            "select count(*) from pragma_table_info('collections') where name='model'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(model_cols, 1);

        let h = Handler::new(pools(dir.path()), 4 << 20, 4);
        for (id, _, json) in &rows {
            let got = h
                .handle(
                    "agent",
                    "vec_get",
                    format!(r#"{{"collection":"mem","id":"{id}"}}"#).as_bytes(),
                )
                .await
                .unwrap();
            let want: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(got["vector"], want, "same numbers for {id}");
            assert_eq!(got["metadata"]["k"], "v");
        }
        // the backup is a readable v0 database
        let bak = backup_path(&dir.path().join("agent.db"));
        let mut conn = SqliteConnectOptions::from_str(&format!("sqlite://{}", bak.display()))
            .unwrap()
            .read_only(true)
            .connect()
            .await
            .unwrap();
        let ty: String = sqlx::query_scalar("select typeof(vector) from vectors limit 1")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(ty, "text");
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let rows = sample_rows();
        let refs: Vec<(&str, &str, &str)> =
            rows.iter().map(|(a, b, c)| (*a, *b, c.as_str())).collect();
        make_v0(dir.path(), "agent", &refs).await;
        let db_path = dir.path().join("agent.db");
        {
            let p = pools(dir.path());
            let pool = p.pool_for("agent").await.unwrap();
            pool.close().await;
        }
        let bak = backup_path(&db_path);
        let m1 = std::fs::metadata(&bak).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));

        let p2 = pools(dir.path());
        let pool = p2.pool_for("agent").await.unwrap();
        let report = run(&pool, &db_path).await.unwrap();
        assert_eq!(report.from_version, 1);
        assert_eq!(report.migrated_rows, 0);
        assert!(report.backup.is_none());
        let m2 = std::fs::metadata(&bak).unwrap().modified().unwrap();
        assert_eq!(m1, m2, "backup untouched on second start");
        let n: i64 = sqlx::query_scalar("select count(*) from vectors")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 3);
    }

    #[tokio::test]
    async fn query_results_identical_after_migration() {
        let dir = tempfile::tempdir().unwrap();
        let rows = sample_rows();
        let refs: Vec<(&str, &str, &str)> =
            rows.iter().map(|(a, b, c)| (*a, *b, c.as_str())).collect();
        make_v0(dir.path(), "agent", &refs).await;
        // what the pre-migration implementation returned: cosine over JSON vectors
        let q = norm(&[0.9, 0.4, 0.1, 0.2]);
        let mut expected: Vec<(String, f32)> = rows
            .iter()
            .map(|(id, _, json)| {
                let v: Vec<f32> = serde_json::from_str(json).unwrap();
                (id.to_string(), v.iter().zip(&q).map(|(a, b)| a * b).sum())
            })
            .collect();
        expected.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let h = Handler::new(pools(dir.path()), 4 << 20, 4);
        let res = h
            .handle(
                "agent",
                "vec_query",
                serde_json::json!({"collection":"mem","vector":q,"top_k":3})
                    .to_string()
                    .as_bytes(),
            )
            .await
            .unwrap();
        let got = res["results"].as_array().unwrap();
        assert_eq!(got.len(), 3);
        for (g, (id, score)) in got.iter().zip(&expected) {
            assert_eq!(g["id"], id.as_str());
            assert!((g["score"].as_f64().unwrap() as f32 - score).abs() < 1e-6);
            assert!(!g["text"].as_str().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn migration_skips_corrupt_row() {
        let dir = tempfile::tempdir().unwrap();
        let rows = sample_rows();
        let mut refs: Vec<(&str, &str, &str)> =
            rows.iter().map(|(a, b, c)| (*a, *b, c.as_str())).collect();
        refs.push(("bad", "broken", "[0.1, oops"));
        make_v0(dir.path(), "agent", &refs).await;
        let p = pools(dir.path());
        let pool = p.pool_for("agent").await.unwrap();
        let ids: Vec<String> = sqlx::query_scalar("select id from vectors order by id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(ids, vec!["a", "b", "c"]);
        let v: i64 = sqlx::query_scalar("pragma user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, 1);
    }

    #[tokio::test]
    async fn fresh_database_gets_v1_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let p = pools(dir.path());
        let pool = p.pool_for("fresh").await.unwrap();
        let v: i64 = sqlx::query_scalar("pragma user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, 1);
        assert!(!backup_path(&dir.path().join("fresh.db")).exists());
    }
}
