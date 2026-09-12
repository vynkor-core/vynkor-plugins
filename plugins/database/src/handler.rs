use crate::db::{self, DbConfig, DbPools};
use crate::request::{parse_request, DbRequest};
use futures_util::TryStreamExt;
use serde_json::{json, Value};
use sqlx::{Column, Either, Executor, Row, SqlitePool, TypeInfo, ValueRef};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEvent {
    pub action: String,
    pub key: String,
}

pub struct Handler {
    pools: DbPools,
    max_value_bytes: usize,
    max_response_bytes: usize,
    start: std::time::Instant,
}

impl Handler {
    pub fn new(config: DbConfig, max_value_bytes: usize, max_response_bytes: usize) -> Self {
        Self {
            pools: DbPools::new(config),
            max_value_bytes,
            max_response_bytes,
            start: std::time::Instant::now(),
        }
    }

    pub fn status_payload(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": crate::PLUGIN_VERSION,
            "uptime_ms": self.start.elapsed().as_millis() as u64,
            "engine_ready": true,
            "last_error": null,
            "counters": {}
        }))
        .unwrap()
    }

    pub async fn handle(
        &self,
        caller_plugin_id: &str,
        action: &str,
        params_json: &[u8],
    ) -> Result<Vec<u8>, String> {
        self.handle_with_events(caller_plugin_id, action, params_json)
            .await
            .map(|(bytes, _event)| bytes)
    }

    pub async fn handle_with_events(
        &self,
        caller_plugin_id: &str,
        action: &str,
        params_json: &[u8],
    ) -> Result<(Vec<u8>, Option<ChangeEvent>), String> {
        if caller_plugin_id.is_empty() {
            return Err("missing caller_plugin_id (rejected before touching any database)".to_string());
        }
        let req = parse_request(action, params_json)?;
        let pool = self.pools.pool_for(caller_plugin_id).await?;

        // Expired rows are swept before every action — including reads and
        // raw db_query — so no accessor ever has to see a key whose TTL has
        // passed (the expiry filters below are belt-and-braces against a row
        // expiring between the sweep and the access).
        db::sweep_expired(&pool).await?;

        let (result, event) = match req {
            DbRequest::Get { key } => (self.handle_get(&pool, &key).await?, None),
            DbRequest::Set { key, value, ttl_ms } => (
                self.handle_set(&pool, &key, &value, ttl_ms).await?,
                Some(ChangeEvent {
                    action: "db_set".into(),
                    key,
                }),
            ),
            DbRequest::Delete { key } => {
                let (deleted, value) = self.handle_delete(&pool, &key).await?;
                let event = deleted.then(|| ChangeEvent {
                    action: "db_delete".into(),
                    key,
                });
                (value, event)
            }
            DbRequest::BatchGet { keys } => (self.handle_batch_get(&pool, &keys).await?, None),
            DbRequest::Query { sql, params } => (self.handle_query(&pool, &sql, &params).await?, None),
            DbRequest::Incr { key, delta } => (
                self.handle_incr(&pool, &key, delta).await?,
                Some(ChangeEvent {
                    action: "db_incr".into(),
                    key,
                }),
            ),
            DbRequest::Keys { prefix } => (self.handle_keys(&pool, &prefix).await?, None),
            DbRequest::Append { key, value } => (
                self.handle_append(&pool, &key, &value).await?,
                Some(ChangeEvent {
                    action: "db_append".into(),
                    key,
                }),
            ),
            DbRequest::Patch { key, path, value } => (
                self.handle_patch(&pool, &key, &path, &value).await?,
                Some(ChangeEvent {
                    action: "db_patch".into(),
                    key,
                }),
            ),
        };

        let bytes =
            serde_json::to_vec(&result).map_err(|e| format!("failed to encode response: {e}"))?;
        Ok((bytes, event))
    }

    async fn handle_get(&self, pool: &SqlitePool, key: &str) -> Result<Value, String> {
        let row: Option<(String,)> = sqlx::query_as(
            "select value from kv where key = ?1 and (expires_at is null or expires_at > ?2)",
        )
        .bind(key)
        .bind(db::now_ms())
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
        match row {
            Some((raw,)) => {
                let value: Value = serde_json::from_str(&raw)
                    .map_err(|e| format!("corrupt stored value for key {key:?}: {e}"))?;
                Ok(json!({"found": true, "value": value}))
            }
            None => Ok(json!({"found": false, "value": null})),
        }
    }

    async fn handle_set(
        &self,
        pool: &SqlitePool,
        key: &str,
        value: &Value,
        ttl_ms: Option<i64>,
    ) -> Result<Value, String> {
        let raw = serde_json::to_string(value).map_err(|e| format!("failed to encode value: {e}"))?;
        if raw.len() > self.max_value_bytes {
            return Err(format!(
                "value exceeds max_value_bytes ({} > {})",
                raw.len(),
                self.max_value_bytes
            ));
        }
        let now_ms = db::now_ms();
        let expires_at = ttl_ms.map(|ttl| now_ms + ttl);
        sqlx::query(
            "insert into kv (key, value, updated_at, expires_at) values (?1, ?2, ?3, ?4) \
             on conflict(key) do update set value = excluded.value, \
             updated_at = excluded.updated_at, expires_at = excluded.expires_at",
        )
        .bind(key)
        .bind(&raw)
        .bind(now_ms)
        .bind(expires_at)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(json!({"ok": true}))
    }

    async fn handle_delete(&self, pool: &SqlitePool, key: &str) -> Result<(bool, Value), String> {
        let result = sqlx::query("delete from kv where key = ?1")
            .bind(key)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok((result.rows_affected() > 0, json!({"deleted": result.rows_affected() > 0})))
    }

    async fn handle_batch_get(&self, pool: &SqlitePool, keys: &[String]) -> Result<Value, String> {
        let mut values = serde_json::Map::new();
        // Track the serialized size as we go and reject once it crosses
        // max_response_bytes, matching db_query. Without this a caller could
        // pull an unbounded blob back in one batch even though every
        // individual value passed the db_set cap (bug #2).
        let mut running_bytes: usize = 0;
        for key in keys {
            let row: Option<(String,)> = sqlx::query_as(
                "select value from kv where key = ?1 and (expires_at is null or expires_at > ?2)",
            )
            .bind(key)
            .bind(db::now_ms())
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;
            let value = match row {
                Some((raw,)) => serde_json::from_str(&raw)
                    .map_err(|e| format!("corrupt stored value for key {key:?}: {e}"))?,
                None => Value::Null,
            };
            // Account for the value plus its key and JSON punctuation.
            running_bytes += serde_json::to_vec(&value).map_err(|e| e.to_string())?.len()
                + key.len()
                + 4;
            if running_bytes > self.max_response_bytes {
                return Err(format!(
                    "batch_get result exceeds max_response_bytes (> {})",
                    self.max_response_bytes
                ));
            }
            values.insert(key.clone(), value);
        }
        Ok(json!({"values": Value::Object(values)}))
    }

    async fn handle_query(&self, pool: &SqlitePool, sql: &str, params: &[Value]) -> Result<Value, String> {
        if db::rejects_attach(sql) {
            return Err("ATTACH is not permitted in db_query statements".to_string());
        }

        // One streaming pass over `fetch_many`, which yields `Either::Right`
        // for each result row and `Either::Left` for the statement's final
        // query result (carrying `rows_affected`). This replaces the old
        // `starts_with("select")` sniff, which misrouted `INSERT ...
        // RETURNING` (a row-producing write) to the execute-only path and
        // dropped its rows (bug #4). Streaming also lets us enforce
        // `max_response_bytes` incrementally instead of after materializing
        // every row (bug #3): `running_bytes` tracks the serialized size of
        // the rows collected so far and bails the moment it crosses the cap,
        // so a runaway `SELECT` can't balloon memory before the check fires.
        let mut query = sqlx::query(sql);
        for p in params {
            query = bind_json_param(query, p);
        }

        let mut stream = pool.fetch_many(query);
        let mut json_rows: Vec<Value> = Vec::new();
        let mut running_bytes: usize = 0;
        let mut rows_affected: u64 = 0;

        while let Some(item) = stream.try_next().await.map_err(|e| e.to_string())? {
            match item {
                Either::Left(result) => {
                    rows_affected += result.rows_affected();
                }
                Either::Right(row) => {
                    let value = row_to_json(&row)?;
                    // Account for the row plus its `,` separator against the
                    // cap before pushing, so we never hold more than one
                    // oversized row's worth past the limit.
                    running_bytes += serde_json::to_vec(&value)
                        .map_err(|e| e.to_string())?
                        .len()
                        + 1;
                    if running_bytes > self.max_response_bytes {
                        return Err(format!(
                            "query result exceeds max_response_bytes (> {})",
                            self.max_response_bytes
                        ));
                    }
                    json_rows.push(value);
                }
            }
        }

        Ok(json!({"rows": json_rows, "rows_affected": rows_affected}))
    }

    async fn handle_incr(&self, pool: &SqlitePool, key: &str, delta: i64) -> Result<Value, String> {
        let now_ms = db::now_ms();
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        let row: Option<(String,)> = sqlx::query_as(
            "select value from kv where key = ?1 and (expires_at is null or expires_at > ?2)",
        )
        .bind(key)
        .bind(now_ms)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        let new_value = match row {
            None => delta,
            Some((raw,)) => {
                let value: Value = serde_json::from_str(&raw)
                    .map_err(|e| format!("corrupt stored value for key {key:?}: {e}"))?;
                match value.as_i64() {
                    Some(old) => old + delta,
                    None => {
                        return Err(format!(
                            "key '{key}' is not a counter: stored value is not an integer"
                        ));
                    }
                }
            }
        };

        // Upsert with only key/value/updated_at in the conflict set: a row
        // that already existed keeps its expires_at unchanged (it was alive
        // a moment ago, so a TTL set earlier still applies).
        sqlx::query(
            "insert into kv (key, value, updated_at) values (?1, ?2, ?3) \
             on conflict(key) do update set value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(new_value.to_string())
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(json!({"ok": true, "value": new_value}))
    }

    async fn handle_keys(&self, pool: &SqlitePool, prefix: &str) -> Result<Value, String> {
        // Escape LIKE wildcards in the prefix so `user:100%` matches only
        // literal `user:100%...` keys. `ESCAPE '\'` makes backslash the
        // escape char, so backslashes themselves are doubled first.
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let rows: Vec<(String,)> = sqlx::query_as("select key from kv where key like ?1 escape '\\' order by key")
            .bind(pattern)
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({"keys": rows.into_iter().map(|(k,)| k).collect::<Vec<String>>()}))
    }

    async fn handle_append(&self, pool: &SqlitePool, key: &str, value: &Value) -> Result<Value, String> {
        let now_ms = db::now_ms();
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        let row: Option<(String,)> = sqlx::query_as(
            "select value from kv where key = ?1 and (expires_at is null or expires_at > ?2)",
        )
        .bind(key)
        .bind(now_ms)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        let (raw, length) = match row {
            None => {
                let arr = json!([value]);
                let raw = serde_json::to_string(&arr).map_err(|e| format!("failed to encode value: {e}"))?;
                (raw, 1usize)
            }
            Some((stored,)) => {
                let mut arr: Value = serde_json::from_str(&stored)
                    .map_err(|e| format!("corrupt stored value for key {key:?}: {e}"))?;
                match arr.as_array_mut() {
                    Some(items) => {
                        items.push(value.clone());
                        let length = items.len();
                        let raw = serde_json::to_string(&arr)
                            .map_err(|e| format!("failed to encode value: {e}"))?;
                        (raw, length)
                    }
                    None => return Err(format!("key '{key}' is not an array: cannot append")),
                }
            }
        };

        if raw.len() > self.max_value_bytes {
            return Err(format!(
                "value exceeds max_value_bytes ({} > {})",
                raw.len(),
                self.max_value_bytes
            ));
        }

        sqlx::query(
            "insert into kv (key, value, updated_at) values (?1, ?2, ?3) \
             on conflict(key) do update set value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(&raw)
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(json!({"ok": true, "length": length}))
    }

    async fn handle_patch(
        &self,
        pool: &SqlitePool,
        key: &str,
        path: &str,
        value: &Value,
    ) -> Result<Value, String> {
        let now_ms = db::now_ms();
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        let existing: Option<(String,)> = sqlx::query_as(
            "select value from kv where key = ?1 and (expires_at is null or expires_at > ?2)",
        )
        .bind(key)
        .bind(now_ms)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        if existing.is_none() {
            return Err(format!("key not found: '{key}'"));
        }

        let value_json =
            serde_json::to_string(value).map_err(|e| format!("failed to encode value: {e}"))?;
        sqlx::query("update kv set value = json_set(value, ?2, json(?3)), updated_at = ?4 where key = ?1")
            .bind(key)
            .bind(path)
            .bind(&value_json)
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.to_ascii_lowercase().contains("json path") {
                    format!("invalid JSON path {path:?}: {msg}")
                } else {
                    msg
                }
            })?;

        let (new_raw,): (String,) = sqlx::query_as("select value from kv where key = ?1")
            .bind(key)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        let new_value: Value = serde_json::from_str(&new_raw)
            .map_err(|e| format!("corrupt stored value for key {key:?}: {e}"))?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(json!({"ok": true, "value": new_value}))
    }
}

fn bind_json_param<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    value: &'q Value,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match value {
        Value::Null => query.bind(None::<String>),
        Value::Bool(b) => query.bind(*b as i64),
        Value::Number(n) if n.is_i64() => query.bind(n.as_i64().unwrap()),
        Value::Number(n) => query.bind(n.as_f64().unwrap_or_default()),
        Value::String(s) => query.bind(s.clone()),
        other => query.bind(other.to_string()),
    }
}

fn row_to_json(row: &sqlx::sqlite::SqliteRow) -> Result<Value, String> {
    let mut obj = serde_json::Map::new();
    for (i, col) in row.columns().iter().enumerate() {
        let raw = row.try_get_raw(i).map_err(|e| e.to_string())?;
        let value = if raw.is_null() {
            Value::Null
        } else {
            match raw.type_info().name() {
                "TEXT" => Value::String(row.try_get::<String, _>(i).map_err(|e| e.to_string())?),
                "INTEGER" => json!(row.try_get::<i64, _>(i).map_err(|e| e.to_string())?),
                "REAL" => json!(row.try_get::<f64, _>(i).map_err(|e| e.to_string())?),
                "BLOB" => {
                    let bytes = row.try_get::<Vec<u8>, _>(i).map_err(|e| e.to_string())?;
                    json!(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes))
                }
                other => return Err(format!("unsupported SQLite column type: {other}")),
            }
        };
        obj.insert(col.name().to_string(), value);
    }
    Ok(Value::Object(obj))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;

    fn handler(dir: &std::path::Path) -> Handler {
        Handler::new(
            DbConfig {
                data_dir: dir.to_path_buf(),
                pool_size: 2,
                busy_timeout_ms: 1000,
                max_db_bytes: 0,
            },
            1024,
            4096,
        )
    }

    #[tokio::test]
    async fn set_then_get_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());

        let set_out = h
            .handle("caller_a", "db_set", br#"{"key": "foo", "value": {"n": 1}}"#)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&set_out).unwrap(),
            serde_json::json!({"ok": true})
        );

        let get_out = h.handle("caller_a", "db_get", br#"{"key": "foo"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&get_out).unwrap(),
            serde_json::json!({"found": true, "value": {"n": 1}})
        );
    }

    #[tokio::test]
    async fn get_missing_key_returns_found_false() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let out = h.handle("caller_a", "db_get", br#"{"key": "nope"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"found": false, "value": null})
        );
    }

    #[tokio::test]
    async fn delete_reports_whether_a_row_was_removed() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "foo", "value": 1}"#)
            .await
            .unwrap();

        let out1 = h.handle("caller_a", "db_delete", br#"{"key": "foo"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out1).unwrap(),
            serde_json::json!({"deleted": true})
        );

        let out2 = h.handle("caller_a", "db_delete", br#"{"key": "foo"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out2).unwrap(),
            serde_json::json!({"deleted": false})
        );
    }

    #[tokio::test]
    async fn batch_get_returns_map_with_nulls_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "a", "value": 1}"#).await.unwrap();

        let out = h
            .handle("caller_a", "db_batch_get", br#"{"keys": ["a", "b"]}"#)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"values": {"a": 1, "b": null}})
        );
    }

    #[tokio::test]
    async fn query_reads_back_through_kv_table() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "foo", "value": "bar"}"#)
            .await
            .unwrap();

        let out = h
            .handle(
                "caller_a",
                "db_query",
                br#"{"sql": "select key, value from kv where key = ?1", "params": ["foo"]}"#,
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["rows"][0]["key"], "foo");
        assert_eq!(v["rows"][0]["value"], "\"bar\"");
    }

    #[tokio::test]
    async fn query_rejects_attach() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let err = h
            .handle(
                "caller_a",
                "db_query",
                br#"{"sql": "attach database 'x.db' as evil"}"#,
            )
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("attach"), "error was: {err}");
    }

    #[tokio::test]
    async fn callers_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "k", "value": "from_a"}"#)
            .await
            .unwrap();
        h.handle("caller_b", "db_set", br#"{"key": "k", "value": "from_b"}"#)
            .await
            .unwrap();

        let a = h.handle("caller_a", "db_get", br#"{"key": "k"}"#).await.unwrap();
        let b = h.handle("caller_b", "db_get", br#"{"key": "k"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&a).unwrap()["value"],
            "from_a"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&b).unwrap()["value"],
            "from_b"
        );
    }

    #[tokio::test]
    async fn set_rejects_oversized_value() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let big_value = serde_json::json!("x".repeat(2000));
        let params = serde_json::to_vec(&serde_json::json!({"key": "foo", "value": big_value})).unwrap();
        let err = h.handle("caller_a", "db_set", &params).await.unwrap_err();
        assert!(err.contains("max_value_bytes"), "error was: {err}");
    }

    #[tokio::test]
    async fn rejects_missing_caller_id() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let err = h.handle("", "db_get", br#"{"key": "foo"}"#).await.unwrap_err();
        assert!(err.contains("caller_plugin_id"), "error was: {err}");
    }

    #[tokio::test]
    async fn rejects_unknown_action() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let err = h.handle("caller_a", "db_frobnicate", b"{}").await.unwrap_err();
        assert!(err.contains("db_frobnicate"), "error was: {err}");
    }

    #[test]
    fn status_payload_reports_inf07_shape() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let v: serde_json::Value = serde_json::from_slice(&h.status_payload()).unwrap();
        assert_eq!(v["version"], crate::PLUGIN_VERSION);
        assert_eq!(v["engine_ready"], true);
        assert_eq!(v["last_error"], serde_json::Value::Null);
        assert_eq!(v["counters"], serde_json::json!({}));
        assert!(v["uptime_ms"].as_u64().is_some());
    }

    // Fix #4: a write with a RETURNING clause is still a row-producing
    // statement. The old `starts_with("select")` heuristic routed it to the
    // `execute()` path, so the returned rows were silently dropped and the
    // caller only ever saw `rows_affected`. This asserts the rows come back.
    #[tokio::test]
    async fn insert_returning_yields_the_returned_rows() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let out = h
            .handle(
                "caller_a",
                "db_query",
                br#"{"sql": "insert into kv (key, value, updated_at) values ('rk', '\"v\"', 0) returning key, updated_at"}"#,
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["rows"][0]["key"], "rk", "RETURNING rows were dropped: {v}");
        assert_eq!(v["rows"][0]["updated_at"], 0);
    }

    // Fix #2: db_batch_get had no response-size cap, unlike db_query. A caller
    // storing many values (each individually under max_value_bytes) could pull
    // an unbounded blob back in one batch. With max_response_bytes = 4096 in
    // the test handler, ten ~900-byte values must trip the cap.
    #[tokio::test]
    async fn batch_get_rejects_oversized_total_response() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let big = "x".repeat(900);
        let mut keys = Vec::new();
        for i in 0..10 {
            let key = format!("k{i}");
            let params =
                serde_json::to_vec(&serde_json::json!({"key": key, "value": big})).unwrap();
            h.handle("caller_a", "db_set", &params).await.unwrap();
            keys.push(key);
        }
        let params = serde_json::to_vec(&serde_json::json!({"keys": keys})).unwrap();
        let err = h.handle("caller_a", "db_batch_get", &params).await.unwrap_err();
        assert!(err.contains("max_response_bytes"), "error was: {err}");
    }

    // Fix #3 regression guard: the streaming rewrite of handle_query must keep
    // rejecting oversized SELECT results. Ten ~900-byte rows serialized exceed
    // the 4096-byte test cap.
    #[tokio::test]
    async fn query_rejects_oversized_select_result() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let big = "x".repeat(900);
        for i in 0..10 {
            let params =
                serde_json::to_vec(&serde_json::json!({"key": format!("k{i}"), "value": big}))
                    .unwrap();
            h.handle("caller_a", "db_set", &params).await.unwrap();
        }
        let err = h
            .handle("caller_a", "db_query", br#"{"sql": "select key, value from kv"}"#)
            .await
            .unwrap_err();
        assert!(err.contains("max_response_bytes"), "error was: {err}");
    }

    // Fix #1: max_value_bytes only guards db_set; a caller can bypass it with a
    // raw INSERT. The real cap is a per-caller disk quota enforced by SQLite
    // (PRAGMA max_page_count). With a 64 KiB quota, a loop of raw-ish writes
    // must eventually be rejected rather than growing the file unbounded.
    #[tokio::test]
    async fn per_caller_disk_quota_rejects_writes_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let h = Handler::new(
            DbConfig {
                data_dir: dir.path().to_path_buf(),
                pool_size: 2,
                busy_timeout_ms: 1000,
                max_db_bytes: 64 * 1024,
            },
            1024,
            4096,
        );
        let big = "x".repeat(900);
        let mut hit_full = false;
        for i in 0..2000 {
            let params =
                serde_json::to_vec(&serde_json::json!({"key": format!("k{i}"), "value": big}))
                    .unwrap();
            if h.handle("caller_full", "db_set", &params).await.is_err() {
                hit_full = true;
                break;
            }
        }
        assert!(hit_full, "expected a disk-full rejection under the 64 KiB quota");
    }

    // Characterization test backing the transactions section of USAGE.md: a
    // single db_query carrying several statements (a `begin; …; commit;`
    // block) runs all of them on one pooled connection, in order, atomically.
    // This is the documented way to get atomicity, since separate db_query
    // calls may each land on a different connection from the pool. It also
    // pins the multi-statement behavior so a future sqlx bump that drops it
    // can't silently break the documented pattern.
    #[tokio::test]
    async fn multi_statement_query_runs_every_statement() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let out = h
            .handle(
                "caller_a",
                "db_query",
                br#"{"sql": "begin; insert into kv (key, value, updated_at) values ('a', '1', 0); insert into kv (key, value, updated_at) values ('b', '2', 0); commit;"}"#,
            )
            .await
            .unwrap();
        // The block committed both rows.
        let count = h
            .handle("caller_a", "db_query", br#"{"sql": "select count(*) as n from kv"}"#)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&count).unwrap();
        assert_eq!(v["rows"][0]["n"], 2, "multi-statement block did not commit both rows: {}, out was {}", v, String::from_utf8_lossy(&out));
    }

    async fn set_raw(pool: &SqlitePool, key: &str, value: &Value) {
        let raw = serde_json::to_string(value).unwrap();
        sqlx::query("insert into kv (key, value, updated_at) values (?1, ?2, ?3)")
            .bind(key)
            .bind(&raw)
            .bind(db::now_ms())
            .execute(pool)
            .await
            .unwrap();
    }

    // v0.3 — db_incr
    #[tokio::test]
    async fn incr_starts_missing_key_at_delta() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let out = h.handle("caller_a", "db_incr", br#"{"key": "views"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"ok": true, "value": 1})
        );
    }

    #[tokio::test]
    async fn incr_accumulates_on_existing_counter() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_incr", br#"{"key": "n", "delta": 10}"#).await.unwrap();
        let out = h.handle("caller_a", "db_incr", br#"{"key": "n"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"ok": true, "value": 11})
        );
    }

    #[tokio::test]
    async fn incr_supports_negative_delta() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "n", "value": 5}"#).await.unwrap();
        let out = h.handle("caller_a", "db_incr", br#"{"key": "n", "delta": -8}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"ok": true, "value": -3})
        );
    }

    #[tokio::test]
    async fn incr_rejects_non_integer_value() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "s", "value": "text"}"#).await.unwrap();
        let err = h.handle("caller_a", "db_incr", br#"{"key": "s"}"#).await.unwrap_err();
        assert_eq!(
            err,
            "key 's' is not a counter: stored value is not an integer",
            "error was: {err}"
        );
    }

    #[tokio::test]
    async fn incr_starts_fresh_on_expired_key() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let pool = h.pools.pool_for("caller_a").await.unwrap();
        set_raw(&pool, "n", &serde_json::json!(41)).await;
        sqlx::query("update kv set expires_at = 1 where key = 'n'")
            .execute(&pool)
            .await
            .unwrap();
        let out = h.handle("caller_a", "db_incr", br#"{"key": "n"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"ok": true, "value": 1})
        );
    }

    // v0.3 — db_keys
    #[tokio::test]
    async fn keys_lists_all_keys_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        for k in ["b", "a", "c"] {
            h.handle("caller_a", "db_set", &format!(r#"{{"key": "{k}", "value": 1}}"#).into_bytes())
                .await
                .unwrap();
        }
        let out = h.handle("caller_a", "db_keys", b"{}").await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"keys": ["a", "b", "c"]})
        );
    }

    #[tokio::test]
    async fn keys_filters_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        for k in ["user:1", "user:2", "meta:1"] {
            h.handle("caller_a", "db_set", &format!(r#"{{"key": "{k}", "value": 1}}"#).into_bytes())
                .await
                .unwrap();
        }
        let out = h.handle("caller_a", "db_keys", br#"{"prefix": "user:"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"keys": ["user:1", "user:2"]})
        );
    }

    #[tokio::test]
    async fn keys_escapes_like_wildcards_in_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        // JSON-encoded keys: `\\` in the JSON text decodes to one literal
        // backslash in the stored key.
        for k in [r#"a%x"#, r#"a_x"#, r#"ax"#, r#"a\\x"#] {
            let params = serde_json::to_vec(&serde_json::json!({"key": k, "value": 1})).unwrap();
            h.handle("caller_a", "db_set", &params).await.unwrap();
        }
        // Prefix "a%" must match only the literal "a%x" key, not "ax".
        let out = h.handle("caller_a", "db_keys", br#"{"prefix": "a%"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"keys": ["a%x"]})
        );
        // Prefix "a_" must match only the literal "a_x" key.
        let out = h.handle("caller_a", "db_keys", br#"{"prefix": "a_"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"keys": ["a_x"]})
        );
        // Prefix "a\" (a literal backslash) must match only "a\x".
        let out = h.handle("caller_a", "db_keys", br#"{"prefix": "a\\"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"keys": [r#"a\\x"#]})
        );
    }

    #[tokio::test]
    async fn keys_returns_empty_list_when_nothing_matches() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "a", "value": 1}"#).await.unwrap();
        let out = h.handle("caller_a", "db_keys", br#"{"prefix": "zzz"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"keys": []})
        );
    }

    // v0.3 — db_append
    #[tokio::test]
    async fn append_creates_array_on_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let out = h.handle("caller_a", "db_append", br#"{"key": "log", "value": 1}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"ok": true, "length": 1})
        );
        let get = h.handle("caller_a", "db_get", br#"{"key": "log"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&get).unwrap()["value"],
            serde_json::json!([1])
        );
    }

    #[tokio::test]
    async fn append_extends_existing_array() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "log", "value": [1, 2]}"#).await.unwrap();
        let out = h
            .handle("caller_a", "db_append", br#"{"key": "log", "value": {"n": 3}}"#)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"ok": true, "length": 3})
        );
        let get = h.handle("caller_a", "db_get", br#"{"key": "log"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&get).unwrap()["value"],
            serde_json::json!([1, 2, {"n": 3}])
        );
    }

    #[tokio::test]
    async fn append_rejects_non_array_value() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "s", "value": 5}"#).await.unwrap();
        let err = h.handle("caller_a", "db_append", br#"{"key": "s", "value": 1}"#).await.unwrap_err();
        assert_eq!(err, "key 's' is not an array: cannot append", "error was: {err}");
    }

    #[tokio::test]
    async fn append_rejects_oversized_result() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let big = serde_json::json!("x".repeat(900));
        let params =
            serde_json::to_vec(&serde_json::json!({"key": "log", "value": big})).unwrap();
        h.handle("caller_a", "db_append", &params).await.unwrap();
        // Second append of another ~900-byte string: the serialized array
        // now exceeds the 1024-byte max_value_bytes test cap.
        let err = h.handle("caller_a", "db_append", &params).await.unwrap_err();
        assert!(err.contains("max_value_bytes"), "error was: {err}");
    }

    // v0.3 — TTL
    #[tokio::test]
    async fn ttl_expired_key_reads_as_missing_after_direct_expiry_write() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "t", "value": 1, "ttl_ms": 60000}"#)
            .await
            .unwrap();
        // Get works while unexpired.
        let get = h.handle("caller_a", "db_get", br#"{"key": "t"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&get).unwrap(),
            serde_json::json!({"found": true, "value": 1})
        );
        // Simulate the TTL passing by writing an already-expired
        // expires_at directly (no sleeps).
        let pool = h.pools.pool_for("caller_a").await.unwrap();
        sqlx::query("update kv set expires_at = ?1 where key = 't'")
            .bind(db::now_ms() - 1)
            .execute(&pool)
            .await
            .unwrap();
        let get = h.handle("caller_a", "db_get", br#"{"key": "t"}"#).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&get).unwrap(),
            serde_json::json!({"found": false, "value": null})
        );
        // The sweep deleted the row.
        let count: (i64,) = sqlx::query_as("select count(*) from kv where key = 't'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "expired row should be swept, not just hidden");
    }

    #[tokio::test]
    async fn ttl_batch_get_mixes_expired_and_live_keys() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "dead", "value": 1, "ttl_ms": 60000}"#)
            .await
            .unwrap();
        h.handle("caller_a", "db_set", br#"{"key": "alive", "value": 2}"#).await.unwrap();
        let pool = h.pools.pool_for("caller_a").await.unwrap();
        sqlx::query("update kv set expires_at = ?1 where key = 'dead'")
            .bind(db::now_ms() - 1)
            .execute(&pool)
            .await
            .unwrap();

        let out = h
            .handle("caller_a", "db_batch_get", br#"{"keys": ["dead", "alive"]}"#)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"values": {"dead": null, "alive": 2}})
        );
    }

    #[tokio::test]
    async fn ttl_zero_and_negative_mean_no_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "z", "value": 1, "ttl_ms": 0}"#)
            .await
            .unwrap();
        h.handle("caller_a", "db_set", br#"{"key": "n", "value": 1, "ttl_ms": -5}"#)
            .await
            .unwrap();
        let pool = h.pools.pool_for("caller_a").await.unwrap();
        let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
            "select key, expires_at from kv where key in ('z', 'n') order by key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        for (_, expires_at) in rows {
            assert_eq!(expires_at, None, "ttl_ms <= 0 must not set expires_at");
        }
        for k in ["z", "n"] {
            let get = h.handle("caller_a", "db_get", &format!(r#"{{"key": "{k}"}}"#).into_bytes()).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&get).unwrap(),
                serde_json::json!({"found": true, "value": 1})
            );
        }
    }

    #[tokio::test]
    async fn ttl_set_overwrites_previous_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "t", "value": 1, "ttl_ms": 60000}"#)
            .await
            .unwrap();
        // Re-set without ttl_ms: expiry must be cleared, not kept.
        h.handle("caller_a", "db_set", br#"{"key": "t", "value": 2}"#).await.unwrap();
        let pool = h.pools.pool_for("caller_a").await.unwrap();
        let row: (Option<i64>,) = sqlx::query_as("select expires_at from kv where key = 't'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, None, "re-set without ttl_ms must clear expires_at");
    }

    // v0.3 — db_patch
    #[tokio::test]
    async fn patch_updates_nested_path_and_returns_new_value() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "doc", "value": {"a": {"b": 1, "c": 2}}}"#)
            .await
            .unwrap();
        let out = h
            .handle("caller_a", "db_patch", br#"{"key": "doc", "path": "$.a.b", "value": 42}"#)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"ok": true, "value": {"a": {"b": 42, "c": 2}}})
        );
    }

    #[tokio::test]
    async fn patch_can_target_array_index() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "doc", "value": [1, 2, 3]}"#)
            .await
            .unwrap();
        let out = h
            .handle("caller_a", "db_patch", br#"{"key": "doc", "path": "$[0]", "value": "x"}"#)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"ok": true, "value": ["x", 2, 3]})
        );
    }

    #[tokio::test]
    async fn patch_missing_key_errors() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        let err = h
            .handle("caller_a", "db_patch", br#"{"key": "nope", "path": "$.a", "value": 1}"#)
            .await
            .unwrap_err();
        assert_eq!(err, "key not found: 'nope'", "error was: {err}");
    }

    #[tokio::test]
    async fn patch_malformed_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "doc", "value": {"a": 1}}"#)
            .await
            .unwrap();
        // `$..a` raises "bad JSON path" in SQLite (unlike `$[`, which is
        // silently ignored and leaves the value unchanged).
        let err = h
            .handle("caller_a", "db_patch", br#"{"key": "doc", "path": "$..a", "value": 1}"#)
            .await
            .unwrap_err();
        assert!(
            err.to_ascii_lowercase().contains("json path"),
            "expected a JSON-path error, got: {err}"
        );
    }

    // v0.3 — change events
    #[tokio::test]
    async fn mutations_return_change_events_and_reads_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());

        let (_out, event) = h
            .handle_with_events("caller_a", "db_set", br#"{"key": "k", "value": 1}"#)
            .await
            .unwrap();
        assert_eq!(
            event,
            Some(ChangeEvent {
                action: "db_set".into(),
                key: "k".into(),
            })
        );

        let (_out, event) = h
            .handle_with_events("caller_a", "db_incr", br#"{"key": "n"}"#)
            .await
            .unwrap();
        assert_eq!(
            event,
            Some(ChangeEvent {
                action: "db_incr".into(),
                key: "n".into(),
            })
        );

        let (_out, event) = h
            .handle_with_events("caller_a", "db_append", br#"{"key": "log", "value": 1}"#)
            .await
            .unwrap();
        assert_eq!(
            event,
            Some(ChangeEvent {
                action: "db_append".into(),
                key: "log".into(),
            })
        );

        let (_out, event) = h
            .handle_with_events("caller_a", "db_patch", br#"{"key": "k", "path": "$.a", "value": 2}"#)
            .await
            .unwrap();
        assert_eq!(
            event,
            Some(ChangeEvent {
                action: "db_patch".into(),
                key: "k".into(),
            })
        );

        for (action, params) in [
            ("db_get", br#"{"key": "k"}"#.as_slice()),
            ("db_keys", b"{}".as_slice()),
            ("db_batch_get", br#"{"keys": ["k"]}"#.as_slice()),
            ("db_query", br#"{"sql": "select 1"}"#.as_slice()),
        ] {
            let (_out, event) = h.handle_with_events("caller_a", action, params).await.unwrap();
            assert_eq!(event, None, "{action} must not produce a change event");
        }
    }

    #[tokio::test]
    async fn delete_event_only_when_a_row_was_removed() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(dir.path());
        h.handle("caller_a", "db_set", br#"{"key": "k", "value": 1}"#).await.unwrap();

        let (_out, event) = h
            .handle_with_events("caller_a", "db_delete", br#"{"key": "k"}"#)
            .await
            .unwrap();
        assert_eq!(
            event,
            Some(ChangeEvent {
                action: "db_delete".into(),
                key: "k".into(),
            })
        );

        let (out, event) = h
            .handle_with_events("caller_a", "db_delete", br#"{"key": "k"}"#)
            .await
            .unwrap();
        assert_eq!(event, None);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"deleted": false})
        );
    }
}


