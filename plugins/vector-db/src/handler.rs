use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{json, Value};
use sqlx::Row;
use tokio::sync::{mpsc, oneshot};

use crate::cache::{CacheConfig, UpsertRow, VecCache};
use crate::codec;
use crate::config::EmbedConfig;
use crate::db::{sanitize_caller_id, sanitize_collection, sanitize_id, DbPools};
use crate::embed::{fake_embed, normalize, validate_and_normalize_vector};
use crate::request::{parse_request, VectorRequest};

#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
    /// Set once `ai` rejected an array `input`; later batches go one by one.
    array_unsupported: Arc<AtomicBool>,
}

pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self {
            tx,
            array_unsupported: Arc::new(AtomicBool::new(false)),
        }
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
            .map_err(|_| "embedding via ai aborted: serve loop shutting down".to_string())?;
        let effective = if timeout_ms == 0 { 10000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("embedding via ai aborted: serve loop shutting down".to_string()),
            Err(_) => Err(format!("ai.embedding timed out after {effective} ms")),
        }
    }
}

fn ai_params(cfg: &EmbedConfig, input: Value) -> Value {
    json!({
        "provider": cfg.provider,
        "model": cfg.model,
        "base_url": cfg.base_url,
        "api_key_env": cfg.api_key_env,
        "input": input,
        "timeout_ms": cfg.timeout_ms,
    })
}

fn parse_vector(v: &Value, what: &str) -> Result<Vec<f32>, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("ai embedding malformed {what}: {e}"))
}

async fn embed_single(rpc: &Rpc, cfg: &EmbedConfig, text: &str) -> Result<Vec<f32>, String> {
    let v = rpc
        .call("embedding", ai_params(cfg, json!(text)), cfg.timeout_ms)
        .await?;
    let arr = v
        .get("embedding")
        .ok_or("ai embedding missing field embedding")?;
    parse_vector(arr, "embedding")
}

async fn embed_array(
    rpc: &Rpc,
    cfg: &EmbedConfig,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    let v = rpc
        .call("embedding", ai_params(cfg, json!(texts)), cfg.timeout_ms)
        .await?;
    let arr = v
        .get("embeddings")
        .ok_or("ai embedding missing field embeddings")?;
    let out: Vec<Vec<f32>> = serde_json::from_value(arr.clone())
        .map_err(|e| format!("ai embedding malformed embeddings: {e}"))?;
    if out.len() != texts.len() {
        return Err(format!(
            "ai embedding returned {} vectors for {} inputs",
            out.len(),
            texts.len()
        ));
    }
    Ok(out)
}

/// Errors that mean "this `ai` does not understand an array input".
fn looks_like_array_unsupported(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("invalid")
        || e.contains("expected a string")
        || e.contains("missing field embeddings")
}

/// Embed `texts` via `ai` in chunks of `cfg.batch`, one request per chunk.
/// An `ai` that rejects array input is remembered and fed one text at a time.
/// Returned vectors are L2-normalized and in input order.
pub async fn embed_many(
    rpc: &Rpc,
    cfg: &EmbedConfig,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(cfg.batch.max(1)) {
        let mut got: Option<Vec<Vec<f32>>> = None;
        if chunk.len() > 1 && !rpc.array_unsupported.load(Ordering::Relaxed) {
            match embed_array(rpc, cfg, chunk).await {
                Ok(v) => got = Some(v),
                Err(e) if looks_like_array_unsupported(&e) => {
                    eprintln!("[vector-db] ai rejected array embedding ({e}); embedding one by one from now on");
                    rpc.array_unsupported.store(true, Ordering::Relaxed);
                }
                Err(e) => return Err(e),
            }
        }
        let vectors = match got {
            Some(v) => v,
            None => {
                let mut v = Vec::with_capacity(chunk.len());
                for t in chunk {
                    v.push(embed_single(rpc, cfg, t).await?);
                }
                v
            }
        };
        out.extend(vectors);
    }
    for v in out.iter_mut() {
        if !normalize(v) {
            return Err("ai embedding zero norm".to_string());
        }
    }
    Ok(out)
}

pub struct Embedded {
    pub vectors: Vec<Vec<f32>>,
    /// True when the deterministic fake embedder produced the vectors.
    pub fake: bool,
}

async fn embed_texts(
    texts: &[String],
    dim_hint: Option<usize>,
    default_dim: usize,
    rpc: Option<&Rpc>,
    embed_cfg: Option<&EmbedConfig>,
) -> Result<Embedded, String> {
    if let (Some(r), Some(cfg)) = (rpc, embed_cfg) {
        if cfg.enabled {
            match embed_many(r, cfg, texts).await {
                Ok(vectors) => {
                    if let Some(d) = dim_hint {
                        if let Some(bad) = vectors.iter().find(|v| v.len() != d) {
                            return Err(format!(
                                "ai embedding dim mismatch: expected {d}, got {}",
                                bad.len()
                            ));
                        }
                    }
                    return Ok(Embedded {
                        vectors,
                        fake: false,
                    });
                }
                Err(e) => {
                    if cfg.fallback == crate::config::EmbedFallback::Error {
                        return Err(format!("ai embedding failed: {e}"));
                    }
                    eprintln!(
                        "[vector-db] ai embedding failed ({}), falling back to fake",
                        e
                    );
                }
            }
        }
    }
    if embed_cfg.is_some_and(|c| c.enabled && c.fallback == crate::config::EmbedFallback::Error) {
        return Err(
            "ai embedding unavailable (no Rpc or ai failure) and fallback=error — refusing fake"
                .to_string(),
        );
    }
    let d = dim_hint.unwrap_or(default_dim);
    Ok(Embedded {
        vectors: texts.iter().map(|t| fake_embed(t, d)).collect(),
        fake: true,
    })
}

/// JSON array of numbers exactly as the f32 values print (0.1, not 0.10000000149).
fn vec_to_value(v: &[f32]) -> Value {
    serde_json::to_string(v)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!([]))
}

fn reembedding_msg(collection: &str, model: &str) -> String {
    format!("collection {collection} is being re-embedded for model {model}")
}

type CollectionInfo = Option<(usize, String)>;

pub struct Handler {
    pools: DbPools,
    max_response_bytes: usize,
    default_dim: usize,
    cache: VecCache,
    /// `(caller, collection)` pairs whose vectors are being recomputed.
    reembedding: Arc<StdMutex<HashSet<(String, String)>>>,
}

impl Handler {
    pub fn new(pools: DbPools, max_response_bytes: usize, default_dim: usize) -> Self {
        Self::with_cache(
            pools,
            max_response_bytes,
            default_dim,
            CacheConfig::default(),
        )
    }

    pub fn with_cache(
        pools: DbPools,
        max_response_bytes: usize,
        default_dim: usize,
        cache: CacheConfig,
    ) -> Self {
        Self {
            pools,
            max_response_bytes,
            default_dim,
            cache: VecCache::new(cache),
            reembedding: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    pub fn cache(&self) -> &VecCache {
        &self.cache
    }

    /// Unload collections idle past the configured window (called by a timer).
    pub fn evict_idle(&self) -> usize {
        self.cache.evict_idle(crate::db::now_ms())
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

    /// Fail fast while a collection is being re-embedded, and start the
    /// re-embed when the collection's model differs from the configured one.
    fn model_guard(
        &self,
        pool: &sqlx::SqlitePool,
        caller: &str,
        collection: &str,
        info: &CollectionInfo,
        rpc: Option<&Rpc>,
        cfg: Option<&EmbedConfig>,
    ) -> Result<(), String> {
        let (Some(rpc), Some(cfg)) = (rpc, cfg) else {
            return Ok(());
        };
        if !cfg.enabled {
            return Ok(());
        }
        let key = (caller.to_string(), collection.to_string());
        if self.reembedding.lock().unwrap().contains(&key) {
            return Err(reembedding_msg(collection, &cfg.model));
        }
        let stale = info
            .as_ref()
            .is_some_and(|(_, m)| !m.is_empty() && *m != cfg.model);
        if !stale {
            return Ok(());
        }
        if self.reembedding.lock().unwrap().insert(key.clone()) {
            let (pool, cache, rpc, cfg, set) = (
                pool.clone(),
                self.cache.clone(),
                rpc.clone(),
                cfg.clone(),
                self.reembedding.clone(),
            );
            tokio::spawn(async move {
                let (caller, collection) = key.clone();
                match reembed_collection(&pool, &cache, &caller, &collection, &rpc, &cfg).await {
                    Ok(n) => eprintln!(
                        "[vector-db] re-embedded {n} rows of {collection} for model {}",
                        cfg.model
                    ),
                    Err(e) => eprintln!("[vector-db] re-embed of {collection} failed: {e}"),
                }
                set.lock().unwrap().remove(&key);
            });
        }
        Err(reembedding_msg(collection, &cfg.model))
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

        if let VectorRequest::Embed { texts } = req {
            let emb = embed_texts(&texts, None, self.default_dim, rpc.as_ref(), embed_cfg).await?;
            let dim = emb.vectors.first().map(|v| v.len()).unwrap_or(0);
            let model = match (emb.fake, embed_cfg) {
                (false, Some(c)) => c.model.clone(),
                _ => "fake".to_string(),
            };
            let vectors: Vec<Value> = emb.vectors.iter().map(|v| vec_to_value(v)).collect();
            return Ok(json!({"vectors": vectors, "model": model, "dim": dim}));
        }

        let pool = self.pools.pool_for(caller_plugin_id).await?;
        let caller = caller_plugin_id;

        match req {
            VectorRequest::Embed { .. } => unreachable!("handled above"),
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

                let info = self.collection_info(&pool, &collection).await?;
                self.model_guard(&pool, caller, &collection, &info, rpc.as_ref(), embed_cfg)?;
                let dim = info.as_ref().map(|(d, _)| *d);
                let mut model: Option<String> = None;
                let vec = if let Some(v) = vector {
                    validate_and_normalize_vector(v, dim)?
                } else {
                    let emb = embed_texts(
                        &[text_val.clone()],
                        dim,
                        self.default_dim,
                        rpc.as_ref(),
                        embed_cfg,
                    )
                    .await?;
                    if !emb.fake {
                        model = embed_cfg.map(|c| c.model.clone());
                    }
                    emb.vectors.into_iter().next().unwrap_or_default()
                };
                if let Some(existing_dim) = dim {
                    if vec.len() != existing_dim {
                        return Err(format!(
                            "dimension mismatch: collection '{}' dim {existing_dim}, got {}",
                            collection,
                            vec.len()
                        ));
                    }
                }
                let n = vec.len();
                self.cache
                    .upsert(
                        &pool,
                        caller,
                        &collection,
                        n,
                        model.as_deref(),
                        vec![UpsertRow {
                            id: id.clone(),
                            text: text_val,
                            vector: vec,
                            metadata_json: metadata_str,
                        }],
                    )
                    .await?;
                Ok(json!({"ok": true, "id": id, "dim": n}))
            }
            VectorRequest::BatchUpsert { collection, docs } => {
                let collection = sanitize_collection(&collection)?.to_string();
                let info = self.collection_info(&pool, &collection).await?;
                self.model_guard(&pool, caller, &collection, &info, rpc.as_ref(), embed_cfg)?;
                let mut dim = info.as_ref().map(|(d, _)| *d);

                // Embed every text-only document in one pass (batched calls to ai).
                let mut texts: Vec<String> = Vec::new();
                for doc in &docs {
                    if doc.vector.is_none() {
                        texts.push(doc.text.clone().unwrap_or_default());
                    }
                }
                let mut model: Option<String> = None;
                let mut embedded = if texts.is_empty() {
                    Vec::new().into_iter()
                } else {
                    let emb =
                        embed_texts(&texts, dim, self.default_dim, rpc.as_ref(), embed_cfg).await?;
                    if !emb.fake {
                        model = embed_cfg.map(|c| c.model.clone());
                    }
                    emb.vectors.into_iter()
                };

                let mut rows = Vec::with_capacity(docs.len());
                for (n, doc) in docs.into_iter().enumerate() {
                    let id = sanitize_id(&doc.id)?.to_string();
                    let text_val = doc.text.unwrap_or_default();
                    let metadata_str = doc
                        .metadata
                        .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string()))
                        .unwrap_or_else(|| "{}".to_string());
                    let vec = match doc.vector {
                        Some(v) => validate_and_normalize_vector(v, dim)?,
                        None => embedded.next().ok_or("embedding count mismatch")?,
                    };
                    match dim {
                        Some(expected) if vec.len() != expected => {
                            return Err(format!(
                                "docs[{n}] dimension mismatch: collection '{collection}' dim {expected}, got {}",
                                vec.len()
                            ));
                        }
                        None => dim = Some(vec.len()),
                        _ => {}
                    }
                    rows.push(UpsertRow {
                        id,
                        text: text_val,
                        vector: vec,
                        metadata_json: metadata_str,
                    });
                }
                let count = rows.len();
                let final_dim = dim.unwrap_or(self.default_dim);
                self.cache
                    .upsert(
                        &pool,
                        caller,
                        &collection,
                        final_dim,
                        model.as_deref(),
                        rows,
                    )
                    .await?;
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
                let info = self.collection_info(&pool, &collection).await?;
                let dim = match &info {
                    Some((d, _)) => *d,
                    None => return Ok(json!({"results": []})),
                };
                self.model_guard(&pool, caller, &collection, &info, rpc.as_ref(), embed_cfg)?;

                let query_vec = if let Some(v) = vector {
                    validate_and_normalize_vector(v, Some(dim))?
                } else {
                    let t = text.unwrap_or_default();
                    let emb =
                        embed_texts(&[t], Some(dim), self.default_dim, rpc.as_ref(), embed_cfg)
                            .await?;
                    emb.vectors.into_iter().next().unwrap_or_default()
                };

                // Exact match on top-level metadata keys; non-object / empty filters match everything.
                let filter_map = filter
                    .as_ref()
                    .and_then(|f| f.as_object())
                    .filter(|m| !m.is_empty());
                let hits = self
                    .cache
                    .query(&pool, caller, &collection, &query_vec, top_k, filter_map)
                    .await?;

                let sql = if include_vector {
                    "select text, metadata, vector from vectors where collection = ?1 and id = ?2"
                } else {
                    "select text, metadata from vectors where collection = ?1 and id = ?2"
                };
                let mut results = Vec::new();
                let mut bytes = 0usize;
                for (vid, score) in hits {
                    let Some(row) = sqlx::query(sql)
                        .bind(&collection)
                        .bind(&vid)
                        .fetch_optional(&pool)
                        .await
                        .map_err(|e| e.to_string())?
                    else {
                        continue; // deleted between scoring and fetch
                    };
                    let vtext: String = row.get("text");
                    let vmeta: String = row.get("metadata");
                    let meta_val: Value = serde_json::from_str(&vmeta).unwrap_or(json!({}));
                    let mut obj = json!({
                        "id": vid,
                        "score": score,
                        "text": vtext,
                        "metadata": meta_val
                    });
                    if include_vector {
                        let blob: Vec<u8> = row.get("vector");
                        let v = codec::decode(&blob)
                            .map_err(|e| format!("corrupt vector for {vid}: {e}"))?;
                        obj["vector"] = vec_to_value(&v);
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
                        let blob: Vec<u8> = r.get("vector");
                        let metadata_str: String = r.get("metadata");
                        let v = codec::decode(&blob)
                            .map_err(|e| format!("corrupt vector for {id}: {e}"))?;
                        let metadata: Value =
                            serde_json::from_str(&metadata_str).unwrap_or(json!({}));
                        Ok(json!({
                            "found": true,
                            "id": id,
                            "text": text,
                            "vector": vec_to_value(&v),
                            "metadata": metadata,
                            "dim": v.len()
                        }))
                    }
                    None => Ok(json!({"found": false})),
                }
            }
            VectorRequest::Delete { collection, id } => {
                let collection = sanitize_collection(&collection)?.to_string();
                let id = sanitize_id(&id)?.to_string();
                let deleted = self.cache.delete(&pool, caller, &collection, &id).await?;
                Ok(json!({"deleted": deleted}))
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
                match self.collection_info(&pool, &collection).await? {
                    Some((d, model)) => {
                        let count: (i64,) =
                            sqlx::query_as("select count(*) from vectors where collection = ?1")
                                .bind(&collection)
                                .fetch_one(&pool)
                                .await
                                .map_err(|e| e.to_string())?;
                        Ok(json!({"count": count.0, "dim": d, "model": model}))
                    }
                    None => Ok(json!({"count": 0, "dim": 0, "model": ""})),
                }
            }
        }
    }

    async fn collection_info(
        &self,
        pool: &sqlx::SqlitePool,
        collection: &str,
    ) -> Result<CollectionInfo, String> {
        let row: Option<(i64, String)> =
            sqlx::query_as("select dim, model from collections where name = ?1")
                .bind(collection)
                .fetch_optional(pool)
                .await
                .map_err(|e| e.to_string())?;
        Ok(row.map(|(d, m)| (d as usize, m)))
    }
}

/// Recompute every vector of a collection from its `text` with the configured
/// model, then record the model (and possibly new dimension) on the collection.
/// Rows without text cannot be re-embedded and are deleted. Returns rows kept.
async fn reembed_collection(
    pool: &sqlx::SqlitePool,
    cache: &VecCache,
    caller: &str,
    collection: &str,
    rpc: &Rpc,
    cfg: &EmbedConfig,
) -> Result<usize, String> {
    const PAGE: i64 = 256;
    let mut last: i64 = 0;
    let mut kept = 0usize;
    let mut new_dim: Option<usize> = None;
    loop {
        let rows = sqlx::query(
            "select rowid, id, text from vectors where collection = ?1 and rowid > ?2 \
             order by rowid limit ?3",
        )
        .bind(collection)
        .bind(last)
        .bind(PAGE)
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
        if rows.is_empty() {
            break;
        }
        let mut ids: Vec<String> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut drop_ids: Vec<String> = Vec::new();
        for r in &rows {
            last = r.get::<i64, _>("rowid");
            let id: String = r.get("id");
            let text: String = r.get("text");
            if text.trim().is_empty() {
                eprintln!(
                    "[vector-db] warn: re-embed of {collection}: deleting row {id} with empty text"
                );
                drop_ids.push(id);
            } else {
                ids.push(id);
                texts.push(text);
            }
        }
        let vectors = if texts.is_empty() {
            Vec::new()
        } else {
            embed_many(rpc, cfg, &texts).await?
        };
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        for id in &drop_ids {
            sqlx::query("delete from vectors where collection = ?1 and id = ?2")
                .bind(collection)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        for (id, v) in ids.iter().zip(&vectors) {
            match new_dim {
                None => new_dim = Some(v.len()),
                Some(d) if d != v.len() => {
                    return Err(format!(
                        "model returned mixed dimensions ({d} and {})",
                        v.len()
                    ))
                }
                _ => {}
            }
            sqlx::query("update vectors set vector = ?3 where collection = ?1 and id = ?2")
                .bind(collection)
                .bind(id)
                .bind(codec::encode(v))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            kept += 1;
        }
        tx.commit().await.map_err(|e| e.to_string())?;
    }
    match new_dim {
        Some(d) => {
            sqlx::query("update collections set model = ?2, dim = ?3 where name = ?1")
                .bind(collection)
                .bind(&cfg.model)
                .bind(d as i64)
                .execute(pool)
                .await
        }
        None => {
            sqlx::query("update collections set model = ?2 where name = ?1")
                .bind(collection)
                .bind(&cfg.model)
                .execute(pool)
                .await
        }
    }
    .map_err(|e| e.to_string())?;
    cache.invalidate(caller, collection).await;
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{DbConfig, DbPools};
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

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
            .handle(
                caller,
                "vec_query",
                br#"{"collection":"mem","text":"hello world","top_k":1}"#,
            )
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
        assert!(
            res["results"][0]["score"].as_f64().unwrap()
                > res["results"][1]["score"].as_f64().unwrap()
        );
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
            .handle(
                "caller_b",
                "vec_query",
                br#"{"collection":"c","text":"hello","top_k":5}"#,
            )
            .await
            .unwrap();
        assert_eq!(res["results"].as_array().unwrap().len(), 0);
        let res_a = h
            .handle(
                "caller_a",
                "vec_query",
                br#"{"collection":"c","text":"hello","top_k":5}"#,
            )
            .await
            .unwrap();
        assert_eq!(res_a["results"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_delete_list_stats() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let caller = "agent";
        h.handle(
            caller,
            "vec_upsert",
            br#"{"collection":"mem","id":"1","text":"hi"}"#,
        )
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
        assert!(list["collections"]
            .as_array()
            .unwrap()
            .contains(&json!("mem")));

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
            .handle(
                caller,
                "vec_query",
                br#"{"collection":"batch","text":"hello","top_k":5}"#,
            )
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
            batch: 64,
        };
        let err = h
            .handle_with_rpc(
                caller,
                "vec_upsert",
                br#"{"collection":"c","id":"1","text":"hello"}"#,
                None,
                Some(&cfg),
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("fallback=error") || err.contains("ai embedding"),
            "err was: {err}"
        );
    }

    // ---- batched embedding, vec_embed, collection model (fake `ai` behind an Rpc) ----

    struct FakeAi {
        rpc: Rpc,
        /// Size of every `embedding` request seen (1 for a single string).
        calls: Arc<StdMutex<Vec<usize>>>,
    }

    fn model_dim(model: &str) -> usize {
        if model == "b" {
            12
        } else {
            8
        }
    }

    /// Stand-in for the `ai` plugin. `array_ok == false` mimics an old `ai`
    /// that only accepts a string `input`.
    fn spawn_fake_ai(array_ok: bool) -> FakeAi {
        let (tx, mut rx) = mpsc::channel::<RpcCall>(16);
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let seen = calls.clone();
        tokio::spawn(async move {
            while let Some(call) = rx.recv().await {
                let p: Value = serde_json::from_slice(&call.params_json).unwrap();
                let model = p["model"].as_str().unwrap().to_string();
                let dim = model_dim(&model);
                // like a real provider: not unit length
                let emb = |t: &str| -> Vec<f32> {
                    fake_embed(&format!("{model}:{t}"), dim).iter().map(|x| x * 2.5).collect()
                };
                let reply = match &p["input"] {
                    Value::String(t) => {
                        seen.lock().unwrap().push(1);
                        Ok(json!({"embedding": emb(t), "dim": dim, "model": model, "usage": {}}))
                    }
                    Value::Array(items) => {
                        seen.lock().unwrap().push(items.len());
                        if array_ok {
                            let v: Vec<Vec<f32>> =
                                items.iter().map(|t| emb(t.as_str().unwrap())).collect();
                            Ok(json!({"embeddings": v, "dim": dim, "model": model, "usage": {}}))
                        } else {
                            Err("ai.embedding failed: invalid JSON: invalid type: sequence, expected a string".to_string())
                        }
                    }
                    _ => Err("bad input".to_string()),
                };
                let _ = call.reply.send(reply);
            }
        });
        FakeAi {
            rpc: Rpc::new(tx),
            calls,
        }
    }

    fn cfg_for(model: &str) -> crate::config::EmbedConfig {
        crate::config::EmbedConfig {
            enabled: true,
            provider: "openai".into(),
            base_url: "http://x/v1".into(),
            model: model.into(),
            api_key_env: "K".into(),
            timeout_ms: 2000,
            fallback: crate::config::EmbedFallback::Error,
            batch: 64,
        }
    }

    #[tokio::test]
    async fn batch_upsert_embeds_in_one_call() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let ai = spawn_fake_ai(true);
        let cfg = cfg_for("a");
        let docs: Vec<Value> = (0..130)
            .map(|i| json!({"id": format!("d{i}"), "text": format!("doc {i}")}))
            .collect();
        let res = h
            .handle_with_rpc(
                "agent",
                "vec_upsert_batch",
                json!({"collection": "c", "docs": docs})
                    .to_string()
                    .as_bytes(),
                Some(ai.rpc.clone()),
                Some(&cfg),
            )
            .await
            .unwrap();
        assert_eq!(res["count"], 130);
        assert_eq!(res["dim"], 8);
        assert_eq!(
            *ai.calls.lock().unwrap(),
            vec![64, 64, 2],
            "3 embedding calls, not 130"
        );
        let stats = h
            .handle("agent", "vec_stats", br#"{"collection":"c"}"#)
            .await
            .unwrap();
        assert_eq!(stats["count"], 130);
        assert_eq!(
            stats["model"], "a",
            "model recorded on first embedded write"
        );
    }

    #[tokio::test]
    async fn batch_falls_back_to_single_when_ai_rejects_array() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let ai = spawn_fake_ai(false);
        let cfg = cfg_for("a");
        let mk = |n: usize, p: &str| {
            let docs: Vec<Value> = (0..n)
                .map(|i| json!({"id": format!("{p}{i}"), "text": format!("doc {p}{i}")}))
                .collect();
            json!({"collection": "c", "docs": docs}).to_string()
        };
        let res = h
            .handle_with_rpc(
                "agent",
                "vec_upsert_batch",
                mk(5, "x").as_bytes(),
                Some(ai.rpc.clone()),
                Some(&cfg),
            )
            .await
            .unwrap();
        assert_eq!(res["count"], 5);
        assert_eq!(
            *ai.calls.lock().unwrap(),
            vec![5, 1, 1, 1, 1, 1],
            "one rejected array try, then singles"
        );
        ai.calls.lock().unwrap().clear();
        h.handle_with_rpc(
            "agent",
            "vec_upsert_batch",
            mk(3, "y").as_bytes(),
            Some(ai.rpc.clone()),
            Some(&cfg),
        )
        .await
        .unwrap();
        assert_eq!(
            *ai.calls.lock().unwrap(),
            vec![1, 1, 1],
            "array rejection is remembered"
        );
        let q = h
            .handle_with_rpc(
                "agent",
                "vec_query",
                br#"{"collection":"c","text":"doc x2","top_k":1}"#,
                Some(ai.rpc.clone()),
                Some(&cfg),
            )
            .await
            .unwrap();
        assert_eq!(q["results"][0]["id"], "x2");
    }

    #[tokio::test]
    async fn vec_embed_returns_normalized_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let ai = spawn_fake_ai(true);
        let cfg = cfg_for("a");
        let res = h
            .handle_with_rpc(
                "agent",
                "vec_embed",
                br#"{"texts":["one","two","three"]}"#,
                Some(ai.rpc.clone()),
                Some(&cfg),
            )
            .await
            .unwrap();
        assert_eq!(res["model"], "a");
        assert_eq!(res["dim"], 8);
        let vs = res["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 3);
        for v in vs {
            let v: Vec<f32> = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(v.len(), 8);
            let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-5, "norm {n}");
        }
        // order preserved: second vector is the embedding of "two"
        let want = fake_embed("a:two", 8); // unit length; the fake ai returned 2.5x that
        let got: Vec<f32> = serde_json::from_value(vs[1].clone()).unwrap();
        for (a, b) in want.iter().zip(&got) {
            assert!((a - b).abs() < 1e-6);
        }
        assert_eq!(*ai.calls.lock().unwrap(), vec![3]);
        // without ai configured: deterministic fake vectors, labelled as such
        let fake = h
            .handle("agent", "vec_embed", br#"{"texts":["one"]}"#)
            .await
            .unwrap();
        assert_eq!(fake["model"], "fake");
        assert_eq!(fake["dim"], 8);
    }

    #[tokio::test]
    async fn vec_embed_rejects_empty_and_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        for bad in [
            json!({"texts": []}),
            json!({"texts": ["ok", "  "]}),
            json!({"texts": (0..65).map(|i| format!("t{i}")).collect::<Vec<_>>()}),
            json!({"texts": ["x".repeat(10001)]}),
            json!({}),
        ] {
            let err = h
                .handle("agent", "vec_embed", bad.to_string().as_bytes())
                .await
                .unwrap_err();
            assert!(
                err.contains("texts") || err.contains("vec_embed"),
                "err was: {err}"
            );
        }
        let ok: Vec<String> = (0..64).map(|i| format!("t{i}")).collect();
        h.handle(
            "agent",
            "vec_embed",
            json!({"texts": ok}).to_string().as_bytes(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn model_change_reembeds_collection() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let ai = spawn_fake_ai(true);
        let (cfg_a, cfg_b) = (cfg_for("a"), cfg_for("b"));
        let docs = json!([
            {"id": "1", "text": "alpha"},
            {"id": "2", "text": "beta"},
            {"id": "3", "text": "gamma"},
        ]);
        h.handle_with_rpc(
            "agent",
            "vec_upsert_batch",
            json!({"collection": "c", "docs": docs})
                .to_string()
                .as_bytes(),
            Some(ai.rpc.clone()),
            Some(&cfg_a),
        )
        .await
        .unwrap();
        // a row without text cannot be re-embedded and is dropped
        h.handle(
            "agent",
            "vec_upsert",
            br#"{"collection":"c","id":"novec","vector":[1,0,0,0,0,0,0,0]}"#,
        )
        .await
        .unwrap();
        let stats = h
            .handle("agent", "vec_stats", br#"{"collection":"c"}"#)
            .await
            .unwrap();
        assert_eq!(
            (
                stats["model"].as_str().unwrap(),
                stats["dim"].as_i64().unwrap(),
                stats["count"].as_i64().unwrap()
            ),
            ("a", 8, 4)
        );

        let q = br#"{"collection":"c","text":"alpha","top_k":3}"#;
        let err = h
            .handle_with_rpc("agent", "vec_query", q, Some(ai.rpc.clone()), Some(&cfg_b))
            .await
            .unwrap_err();
        assert!(
            err.contains("is being re-embedded for model b"),
            "err was: {err}"
        );
        assert!(err.contains("collection c"), "err was: {err}");

        // wait for the background re-embed, then search works with the new model
        let mut res = None;
        for _ in 0..100 {
            match h
                .handle_with_rpc("agent", "vec_query", q, Some(ai.rpc.clone()), Some(&cfg_b))
                .await
            {
                Ok(v) => {
                    res = Some(v);
                    break;
                }
                Err(e) => assert!(e.contains("re-embedded"), "unexpected error: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let res = res.expect("re-embed finished");
        assert_eq!(res["results"][0]["id"], "1");
        assert!(res["results"][0]["score"].as_f64().unwrap() > 0.99);
        let stats = h
            .handle("agent", "vec_stats", br#"{"collection":"c"}"#)
            .await
            .unwrap();
        assert_eq!(
            (
                stats["model"].as_str().unwrap(),
                stats["dim"].as_i64().unwrap(),
                stats["count"].as_i64().unwrap()
            ),
            ("b", 12, 3)
        );
        // old-model vectors are gone: the stored vector has the new dimension
        let got = h
            .handle("agent", "vec_get", br#"{"collection":"c","id":"2"}"#)
            .await
            .unwrap();
        assert_eq!(got["dim"], 12);
    }
}
