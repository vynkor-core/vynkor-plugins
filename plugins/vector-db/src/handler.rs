use serde_json::{json, Value};
use sqlx::Row;
use tokio::sync::{mpsc, oneshot};

use crate::config::EmbedConfig;
use crate::db::{sanitize_caller_id, sanitize_collection, sanitize_id, DbPools};
use crate::embed::{cosine_similarity, fake_embed, validate_and_normalize_vector};
use crate::request::{parse_request, VectorRequest};

#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
}

pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self { tx }
    }
    pub async fn call(&self, action: &str, params: Value, timeout_ms: u32) -> Result<Value, String> {
        let params_json = serde_json::to_vec(&params).map_err(|e| format!("failed to encode {action} params: {e}"))?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RpcCall {
                action: action.to_string(),
                params_json,
                timeout_ms,
                reply,
            })
            .await
            .map_err(|_| format!("embedding via ai aborted: serve loop shutting down"))?;
        let effective = if timeout_ms == 0 { 10000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("embedding via ai aborted: serve loop shutting down".to_string()),
            Err(_) => Err(format!("ai.embedding timed out after {effective} ms")),
        }
    }
}

async fn embed_via_ai_or_fake(
    text: &str,
    dim_hint: Option<usize>,
    default_dim: usize,
    rpc: Option<&Rpc>,
    embed_cfg: Option<&EmbedConfig>,
) -> Result<Vec<f32>, String> {
    if let (Some(r), Some(cfg)) = (rpc, embed_cfg) {
        if cfg.enabled {
            let params = serde_json::json!({
                "provider": cfg.provider,
                "model": cfg.model,
                "base_url": cfg.base_url,
                "api_key_env": cfg.api_key_env,
                "input": text,
                "timeout_ms": cfg.timeout_ms,
            });
            match r.call("embedding", params, cfg.timeout_ms).await {
                Ok(v) => {
                    let arr = v.get("embedding").ok_or("ai embedding missing field embedding")?;
                    let vec: Vec<f32> = serde_json::from_value(arr.clone())
                        .map_err(|e| format!("ai embedding malformed embedding: {e}"))?;
                    if let Some(d) = dim_hint {
                        if vec.len() != d {
                            return Err(format!("ai embedding dim mismatch: expected {d}, got {}", vec.len()));
                        }
                    }
                    let mut normed = vec;
                    if !crate::embed::normalize(&mut normed) {
                        return Err("ai embedding zero norm".to_string());
                    }
                    return Ok(normed);
                }
                Err(e) => {
                    if cfg.fallback == crate::config::EmbedFallback::Error {
                        return Err(format!("ai embedding failed: {e}"));
                    }
                    eprintln!("[vector-db] ai embedding failed ({}), falling back to fake", e);
                }
            }
        }
    }
    if embed_cfg.is_some_and(|c| c.enabled && c.fallback == crate::config::EmbedFallback::Error) {
        return Err("ai embedding unavailable (no Rpc or ai failure) and fallback=error — refusing fake".to_string());
    }
    let d = dim_hint.unwrap_or(default_dim);
    Ok(fake_embed(text, d))
}

pub struct Handler {
    pools: DbPools,
    max_response_bytes: usize,
    default_dim: usize,
}

impl Handler {
    pub fn new(pools: DbPools, max_response_bytes: usize, default_dim: usize) -> Self {
        Self {
            pools,
            max_response_bytes,
            default_dim,
        }
    }

    pub async fn handle(
        &self,
        caller_plugin_id: &str,
        action: &str,
        params_json: &[u8],
    ) -> Result<Value, String> {
        self.handle_inner(caller_plugin_id, action, params_json, None, None)
            .await
    }

    pub async fn handle_with_rpc(
        &self,
        caller_plugin_id: &str,
        action: &str,
        params_json: &[u8],
        rpc: Option<Rpc>,
        embed_cfg: Option<&EmbedConfig>,
    ) -> Result<Value, String> {
        self.handle_inner(caller_plugin_id, action, params_json, rpc, embed_cfg)
            .await
    }

    async fn handle_inner(
        &self,
        caller_plugin_id: &str,
        action: &str,
        params_json: &[u8],
        rpc: Option<Rpc>,
        embed_cfg: Option<&EmbedConfig>,
    ) -> Result<Value, String> {
        sanitize_caller_id(caller_plugin_id)?;
        let req = parse_request(action, params_json)?;
        let pool = self.pools.pool_for(caller_plugin_id).await?;

        match req {
            VectorRequest::Upsert {
                collection,
                id,
                text,
                vector,
                metadata,
            } => {
                let collection = sanitize_collection(&collection)?.to_string();
                let id = sanitize_id(&id)?.to_string();
                let text_val = text.unwrap_or_default();
                let metadata_str = metadata
                    .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string()))
                    .unwrap_or_else(|| "{}".to_string());

                // Resolve dimension and vector — via ai if configured, else fake
                let dim = self.resolve_collection_dim(&pool, &collection).await?;
                let vec = if let Some(v) = vector {
                    validate_and_normalize_vector(v, dim)?
                } else {
                    let d = dim.unwrap_or(self.default_dim);
                    embed_via_ai_or_fake(&text_val, Some(d), self.default_dim, rpc.as_ref(), embed_cfg).await?
                };

                // Check dim consistency if collection exists — handle race by INSERT OR IGNORE
                if let Some(existing_dim) = dim {
                    if vec.len() != existing_dim {
                        return Err(format!(
                            "dimension mismatch: collection '{}' dim {existing_dim}, got {}",
                            collection,
                            vec.len()
                        ));
                    }
                } else {
                    // Create collection with this dim; race-safe (concurrent first-writes)
                    let res = sqlx::query(
                        "insert or ignore into collections (name, dim, created_at) values (?1, ?2, ?3)",
                    )
                    .bind(&collection)
                    .bind(vec.len() as i64)
                    .bind(crate::db::now_ms())
                    .execute(&pool)
                    .await
                    .map_err(|e| e.to_string())?;
                    if res.rows_affected() == 0 {
                        // Another task created it concurrently — verify dim matches
                        let existing = self.resolve_collection_dim(&pool, &collection).await?;
                        if let Some(existing_dim) = existing {
                            if vec.len() != existing_dim {
                                return Err(format!(
                                    "dimension mismatch: collection '{}' dim {existing_dim}, got {}",
                                    collection,
                                    vec.len()
                                ));
                            }
                        }
                    }
                }

                let vector_json = serde_json::to_string(&vec).map_err(|e| e.to_string())?;
                if vector_json.len() > 1024 * 1024 {
                    return Err("vector too large".to_string());
                }

                sqlx::query(
                    "insert into vectors (collection, id, text, vector, metadata, updated_at) values (?1, ?2, ?3, ?4, ?5, ?6) \
                     on conflict(collection, id) do update set text=excluded.text, vector=excluded.vector, metadata=excluded.metadata, updated_at=excluded.updated_at"
                )
                .bind(&collection)
                .bind(&id)
                .bind(&text_val)
                .bind(&vector_json)
                .bind(&metadata_str)
                .bind(crate::db::now_ms())
                .execute(&pool)
                .await
                .map_err(|e| e.to_string())?;

                Ok(json!({"ok": true, "id": id, "dim": vec.len()}))
            }
            VectorRequest::BatchUpsert { collection, docs } => {
                let collection = sanitize_collection(&collection)?.to_string();
                let mut dim = self.resolve_collection_dim(&pool, &collection).await?;
                let mut count = 0usize;
                let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
                for doc in docs {
                    let id = sanitize_id(&doc.id)?.to_string();
                    let text_val = doc.text.unwrap_or_default();
                    let metadata_str = doc
                        .metadata
                        .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string()))
                        .unwrap_or_else(|| "{}".to_string());
                    let vec = if let Some(v) = doc.vector {
                        validate_and_normalize_vector(v, dim)?
                    } else {
                        let d = dim.unwrap_or(self.default_dim);
                        let emb = embed_via_ai_or_fake(&text_val, Some(d), self.default_dim, rpc.as_ref(), embed_cfg).await?;
                        if dim.is_none() {
                            dim = Some(emb.len());
                        }
                        emb
                    };
                    if let Some(expected) = dim {
                        if vec.len() != expected {
                            return Err(format!(
                                "docs[{}] dimension mismatch: collection '{}' dim {expected}, got {}",
                                count, collection, vec.len()
                            ));
                        }
                    }
                    let vector_json = serde_json::to_string(&vec).map_err(|e| e.to_string())?;
                    sqlx::query(
                        "insert into vectors (collection, id, text, vector, metadata, updated_at) values (?1, ?2, ?3, ?4, ?5, ?6) \
                         on conflict(collection, id) do update set text=excluded.text, vector=excluded.vector, metadata=excluded.metadata, updated_at=excluded.updated_at"
                    )
                    .bind(&collection)
                    .bind(&id)
                    .bind(&text_val)
                    .bind(&vector_json)
                    .bind(&metadata_str)
                    .bind(crate::db::now_ms())
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())?;
                    count += 1;
                }
                if let Some(d) = dim {
                    sqlx::query("insert or ignore into collections (name, dim, created_at) values (?1, ?2, ?3)")
                        .bind(&collection)
                        .bind(d as i64)
                        .bind(crate::db::now_ms())
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                tx.commit().await.map_err(|e| e.to_string())?;
                let final_dim = dim.unwrap_or(self.default_dim);
                Ok(json!({"ok": true, "count": count, "dim": final_dim}))
            }
            VectorRequest::Query {
                collection,
                text,
                vector,
                top_k,
                include_vector,
                filter,
            } => {
                let collection = sanitize_collection(&collection)?.to_string();
                let pool_dim = self.resolve_collection_dim(&pool, &collection).await?;
                let dim = match pool_dim {
                    Some(d) => d,
                    None => return Ok(json!({"results": []})),
                };

                let query_vec = if let Some(v) = vector {
                    validate_and_normalize_vector(v, Some(dim))?
                } else {
                    let t = text.unwrap_or_default();
                    embed_via_ai_or_fake(&t, Some(dim), self.default_dim, rpc.as_ref(), embed_cfg).await?
                };

                // Load all vectors in collection
                let rows = sqlx::query(
                    "select id, text, vector, metadata from vectors where collection = ?1",
                )
                .bind(&collection)
                .fetch_all(&pool)
                .await
                .map_err(|e| e.to_string())?;

                let mut scored: Vec<(String, f32, String, String)> = Vec::new();
                for row in rows {
                    let vid: String = row.get("id");
                    let vtext: String = row.get("text");
                    let vvec_json: String = row.get("vector");
                    let vmeta: String = row.get("metadata");

                    // Filter by metadata if provided (exact match on top-level keys)
                    if let Some(ref filt) = filter {
                        if let Some(fobj) = filt.as_object() {
                            if !fobj.is_empty() {
                                let meta_val: Value =
                                    serde_json::from_str(&vmeta).unwrap_or(json!({}));
                                let mut matches = true;
                                for (k, fv) in fobj {
                                    if meta_val.get(k) != Some(fv) {
                                        matches = false;
                                        break;
                                    }
                                }
                                if !matches {
                                    continue;
                                }
                            }
                        }
                    }

                    let stored: Vec<f32> =
                        serde_json::from_str(&vvec_json).map_err(|e| format!("corrupt vector for {vid}: {e}"))?;
                    let score = cosine_similarity(&query_vec, &stored);
                    scored.push((vid, score, vtext, vmeta));
                }

                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                scored.truncate(top_k);

                // Estimate response size
                let mut results = Vec::new();
                let mut bytes = 0usize;
                for (vid, score, vtext, vmeta) in scored {
                    let meta_val: Value = serde_json::from_str(&vmeta).unwrap_or(json!({}));
                    let mut obj = json!({
                        "id": vid,
                        "score": score,
                        "text": vtext,
                        "metadata": meta_val
                    });
                    if include_vector {
                        // re-fetch vector for include
                        let vvec_json: String = sqlx::query_scalar(
                            "select vector from vectors where collection = ?1 and id = ?2",
                        )
                        .bind(&collection)
                        .bind(&vid)
                        .fetch_one(&pool)
                        .await
                        .map_err(|e| e.to_string())?;
                        let vec_val: Value = serde_json::from_str(&vvec_json).unwrap_or(json!([]));
                        obj["vector"] = vec_val;
                    }
                    bytes += serde_json::to_vec(&obj).unwrap().len() + 1;
                    if bytes > self.max_response_bytes {
                        return Err(format!(
                            "query result exceeds max_response_bytes (> {})",
                            self.max_response_bytes
                        ));
                    }
                    results.push(obj);
                }

                Ok(json!({"results": results}))
            }
            VectorRequest::Get { collection, id } => {
                let collection = sanitize_collection(&collection)?.to_string();
                let id = sanitize_id(&id)?.to_string();
                let row = sqlx::query(
                    "select text, vector, metadata from vectors where collection = ?1 and id = ?2",
                )
                .bind(&collection)
                .bind(&id)
                .fetch_optional(&pool)
                .await
                .map_err(|e| e.to_string())?;

                match row {
                    Some(r) => {
                        let text: String = r.get("text");
                        let vector_json: String = r.get("vector");
                        let metadata_str: String = r.get("metadata");
                        let vector: Value =
                            serde_json::from_str(&vector_json).unwrap_or(json!([]));
                        let metadata: Value =
                            serde_json::from_str(&metadata_str).unwrap_or(json!({}));
                        let dim = vector.as_array().map(|a| a.len()).unwrap_or(0);
                        Ok(json!({
                            "found": true,
                            "id": id,
                            "text": text,
                            "vector": vector,
                            "metadata": metadata,
                            "dim": dim
                        }))
                    }
                    None => Ok(json!({"found": false})),
                }
            }
            VectorRequest::Delete { collection, id } => {
                let collection = sanitize_collection(&collection)?.to_string();
                let id = sanitize_id(&id)?.to_string();
                let res = sqlx::query("delete from vectors where collection = ?1 and id = ?2")
                    .bind(&collection)
                    .bind(&id)
                    .execute(&pool)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!({"deleted": res.rows_affected() > 0}))
            }
            VectorRequest::List { prefix } => {
                let rows: Vec<(String,)> = if prefix.is_empty() {
                    sqlx::query_as("select name from collections order by name")
                        .fetch_all(&pool)
                        .await
                        .map_err(|e| e.to_string())?
                } else {
                    let pattern = format!("{prefix}%");
                    sqlx::query_as(
                        "select name from collections where name like ?1 escape '\\' order by name",
                    )
                    .bind(pattern)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| e.to_string())?
                };
                let collections: Vec<String> = rows.into_iter().map(|(n,)| n).collect();
                Ok(json!({"collections": collections}))
            }
            VectorRequest::Stats { collection } => {
                let collection = sanitize_collection(&collection)?.to_string();
                let dim: Option<(i64,)> =
                    sqlx::query_as("select dim from collections where name = ?1")
                        .bind(&collection)
                        .fetch_optional(&pool)
                        .await
                        .map_err(|e| e.to_string())?;
                match dim {
                    Some((d,)) => {
                        let count: (i64,) = sqlx::query_as(
                            "select count(*) from vectors where collection = ?1",
                        )
                        .bind(&collection)
                        .fetch_one(&pool)
                        .await
                        .map_err(|e| e.to_string())?;
                        Ok(json!({"count": count.0, "dim": d}))
                    }
                    None => Ok(json!({"count": 0, "dim": 0})),
                }
            }
        }
    }

    async fn resolve_collection_dim(
        &self,
        pool: &sqlx::SqlitePool,
        collection: &str,
    ) -> Result<Option<usize>, String> {
        // An empty collection has no dimension worth keeping: drop its row so
        // the next write sets it. Otherwise a collection created under one
        // embedding model (384-d all-minilm) rejects every write after the
        // model changes (768-d nomic) — forever, since nothing ever lands.
        sqlx::query(
            "delete from collections where name = ?1 \
             and not exists (select 1 from vectors where collection = ?1)",
        )
        .bind(collection)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
        let row: Option<(i64,)> =
            sqlx::query_as("select dim from collections where name = ?1")
                .bind(collection)
                .fetch_optional(pool)
                .await
                .map_err(|e| e.to_string())?;
        Ok(row.map(|(d,)| d as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{DbConfig, DbPools};
    use serde_json::json;

    fn make_handler(dir: &std::path::Path) -> Handler {
        let pools = DbPools::new(DbConfig {
            data_dir: dir.to_path_buf(),
            pool_size: 2,
            busy_timeout_ms: 2000,
            max_db_bytes: 0,
        });
        Handler::new(pools, 4 * 1024 * 1024, 8)
    }

    #[tokio::test]
    async fn upsert_and_query_text() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        h.handle(
            caller,
            "vec_upsert",
            br#"{"collection":"mem","id":"1","text":"hello world"}"#,
        )
        .await
        .unwrap();
        h.handle(
            caller,
            "vec_upsert",
            br#"{"collection":"mem","id":"2","text":"goodbye world"}"#,
        )
        .await
        .unwrap();
        let res = h
            .handle(caller, "vec_query", br#"{"collection":"mem","text":"hello world","top_k":1}"#)
            .await
            .unwrap();
        assert_eq!(res["results"][0]["id"], "1");
        assert!(res["results"][0]["score"].as_f64().unwrap() > 0.9);
    }

    #[tokio::test]
    async fn upsert_with_vector_and_query_by_vector() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        h.handle(
            caller,
            "vec_upsert",
            br#"{"collection":"c","id":"a","vector":[1,0,0,0,0,0,0,0]}"#,
        )
        .await
        .unwrap();
        h.handle(
            caller,
            "vec_upsert",
            br#"{"collection":"c","id":"b","vector":[0,1,0,0,0,0,0,0]}"#,
        )
        .await
        .unwrap();
        let res = h
            .handle(
                caller,
                "vec_query",
                br#"{"collection":"c","vector":[1,0,0,0,0,0,0,0],"top_k":2}"#,
            )
            .await
            .unwrap();
        assert_eq!(res["results"][0]["id"], "a");
        assert_eq!(res["results"][1]["id"], "b");
        assert!(res["results"][0]["score"].as_f64().unwrap() > res["results"][1]["score"].as_f64().unwrap());
    }

    #[tokio::test]
    async fn dimension_mismatch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        h.handle(
            caller,
            "vec_upsert",
            br#"{"collection":"c","id":"1","vector":[1,0,0,0,0,0,0,0]}"#,
        )
        .await
        .unwrap();
        let err = h
            .handle(
                caller,
                "vec_upsert",
                br#"{"collection":"c","id":"2","vector":[1,0,0]}"#,
            )
            .await
            .unwrap_err();
        assert!(err.contains("dimension mismatch"), "err was: {err}");
    }

    #[tokio::test]
    async fn emptied_collection_takes_the_next_dimension() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        h.handle(caller, "vec_upsert", br#"{"collection":"c","id":"1","vector":[1,0,0]}"#)
            .await
            .unwrap();
        h.handle(caller, "vec_delete", br#"{"collection":"c","id":"1"}"#).await.unwrap();
        h.handle(caller, "vec_upsert", br#"{"collection":"c","id":"2","vector":[1,0,0,0,0]}"#)
            .await
            .expect("an empty collection must not pin its old dimension");
        let err = h
            .handle(caller, "vec_upsert", br#"{"collection":"c","id":"3","vector":[1,0,0]}"#)
            .await
            .unwrap_err();
        assert!(err.contains("dimension mismatch"), "a non-empty one still does: {err}");
    }

    #[tokio::test]
    async fn per_caller_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        h.handle(
            "caller_a",
            "vec_upsert",
            br#"{"collection":"c","id":"1","text":"hello"}"#,
        )
        .await
        .unwrap();
        let res = h
            .handle("caller_b", "vec_query", br#"{"collection":"c","text":"hello","top_k":5}"#)
            .await
            .unwrap();
        assert_eq!(res["results"].as_array().unwrap().len(), 0);
        let res_a = h
            .handle("caller_a", "vec_query", br#"{"collection":"c","text":"hello","top_k":5}"#)
            .await
            .unwrap();
        assert_eq!(res_a["results"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_delete_list_stats() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        h.handle(caller, "vec_upsert", br#"{"collection":"mem","id":"1","text":"hi"}"#)
            .await
            .unwrap();
        let get = h
            .handle(caller, "vec_get", br#"{"collection":"mem","id":"1"}"#)
            .await
            .unwrap();
        assert_eq!(get["found"], true);
        assert_eq!(get["id"], "1");

        let stats = h
            .handle(caller, "vec_stats", br#"{"collection":"mem"}"#)
            .await
            .unwrap();
        assert_eq!(stats["count"], 1);

        let list = h.handle(caller, "vec_list", b"{}").await.unwrap();
        assert!(list["collections"].as_array().unwrap().contains(&json!("mem")));

        let del = h
            .handle(caller, "vec_delete", br#"{"collection":"mem","id":"1"}"#)
            .await
            .unwrap();
        assert_eq!(del["deleted"], true);
        let del2 = h
            .handle(caller, "vec_delete", br#"{"collection":"mem","id":"1"}"#)
            .await
            .unwrap();
        assert_eq!(del2["deleted"], false);

        let get2 = h
            .handle(caller, "vec_get", br#"{"collection":"mem","id":"1"}"#)
            .await
            .unwrap();
        assert_eq!(get2["found"], false);
    }

    #[tokio::test]
    async fn filter_and_topk() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        h.handle(
            caller,
            "vec_upsert",
            br#"{"collection":"c","id":"1","text":"hello","metadata":{"tag":"a"}}"#,
        )
        .await
        .unwrap();
        h.handle(
            caller,
            "vec_upsert",
            br#"{"collection":"c","id":"2","text":"hello","metadata":{"tag":"b"}}"#,
        )
        .await
        .unwrap();
        let res = h
            .handle(
                caller,
                "vec_query",
                br#"{"collection":"c","text":"hello","top_k":5,"filter":{"tag":"a"}}"#,
            )
            .await
            .unwrap();
        assert_eq!(res["results"].as_array().unwrap().len(), 1);
        assert_eq!(res["results"][0]["id"], "1");
    }

    #[tokio::test]
    async fn handles_concurrent_caller_isolation_under_load() {
        let dir = tempfile::tempdir().unwrap();
        let h = std::sync::Arc::new(make_handler(dir.path()));
        let mut tasks = Vec::new();
        for caller_n in 0..4 {
            let h2 = h.clone();
            tasks.push(tokio::spawn(async move {
                let caller = format!("caller_{caller_n}");
                for i in 0..10 {
                    let params = serde_json::json!({
                        "collection": "c",
                        "id": format!("doc:{i}"),
                        "text": format!("text {caller_n} {i}")
                    });
                    let bytes = serde_json::to_vec(&params).unwrap();
                    h2.handle(&caller, "vec_upsert", &bytes).await.unwrap();
                }
                let q = serde_json::json!({"collection":"c","text":"text","top_k":20});
                let res = h2
                    .handle(&caller, "vec_query", &serde_json::to_vec(&q).unwrap())
                    .await
                    .unwrap();
                assert_eq!(res["results"].as_array().unwrap().len(), 10);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    }

    #[tokio::test]
    async fn batch_upsert_is_atomic_and_faster() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        let res = h
            .handle(
                caller,
                "vec_upsert_batch",
                br#"{"collection":"batch","docs":[{"id":"1","text":"hello"},{"id":"2","text":"world"},{"id":"3","vector":[0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8]}]}"#,
            )
            .await
            .unwrap();
        assert_eq!(res["count"], 3);
        assert_eq!(res["dim"], 8);
        let stats = h
            .handle(caller, "vec_stats", br#"{"collection":"batch"}"#)
            .await
            .unwrap();
        assert_eq!(stats["count"], 3);
        let q = h
            .handle(caller, "vec_query", br#"{"collection":"batch","text":"hello","top_k":5}"#)
            .await
            .unwrap();
        assert_eq!(q["results"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn batch_rejects_dim_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        h.handle(
            caller,
            "vec_upsert",
            br#"{"collection":"c","id":"1","vector":[1,0,0,0,0,0,0,0]}"#,
        )
        .await
        .unwrap();
        let err = h
            .handle(
                caller,
                "vec_upsert_batch",
                br#"{"collection":"c","docs":[{"id":"2","vector":[1,0,0]}]}"#,
            )
            .await
            .unwrap_err();
        assert!(err.contains("dimension mismatch"), "err was: {err}");
    }

    #[tokio::test]
    async fn fallback_error_when_ai_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        let cfg = crate::config::EmbedConfig {
            enabled: true,
            provider: "openai".into(),
            base_url: "http://localhost:11434/v1".into(),
            model: "nomic-embed-text".into(),
            api_key_env: "OLLAMA_API_KEY".into(),
            timeout_ms: 100,
            fallback: crate::config::EmbedFallback::Error,
        };
        let err = h
            .handle_with_rpc(caller, "vec_upsert", br#"{"collection":"c","id":"1","text":"hello"}"#, None, Some(&cfg))
            .await
            .unwrap_err();
        assert!(err.contains("fallback=error") || err.contains("ai embedding"), "err was: {err}");
    }
}
