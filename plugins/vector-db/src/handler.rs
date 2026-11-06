use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{json, Value};
use sqlx::Row;
use tokio::sync::{mpsc, oneshot};

use crate::cache::{CacheConfig, SwapOutcome, UpsertRow, VecCache};
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
    // The old `ai` parsed `input` as Option<String>, so an array fails with
    // serde's "invalid type: sequence, expected a string". Anything else
    // (auth, rate limit, transport) must not flip the flag.
    e.contains("invalid type: sequence")
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

/// Fake vectors must never be mixed into a collection that holds real-model
/// vectors (nor used to query it): they would be persisted or matched as noise.
fn refuse_fake_for_real_collection(
    collection: &str,
    info: &CollectionInfo,
    fake: bool,
) -> Result<(), String> {
    if let (true, Some((_, model))) = (fake, info) {
        if !model.is_empty() {
            return Err(format!(
                "ai embedding unavailable; refusing fake vectors for collection {collection} (model {model})"
            ));
        }
    }
    Ok(())
}

const REEMBED_BACKOFF_BASE_MS: i64 = 60_000;
const REEMBED_BACKOFF_MAX_MS: i64 = 30 * 60_000;

/// Per-collection re-embed bookkeeping.
#[derive(Debug, Clone, Default)]
struct ReembedState {
    running: bool,
    fails: u32,
    /// Do not start another attempt before this unix-ms time.
    retry_after_ms: i64,
}

type ReembedMap = Arc<StdMutex<HashMap<(String, String), ReembedState>>>;

fn lock_map(m: &ReembedMap) -> std::sync::MutexGuard<'_, HashMap<(String, String), ReembedState>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Held by the re-embed task; on drop (finish, error or panic) the collection
/// stops counting as running, and a failure arms an exponential backoff.
struct ReembedGuard {
    map: ReembedMap,
    key: (String, String),
    ok: bool,
    base_ms: i64,
}

impl ReembedGuard {
    fn succeed(mut self) {
        self.ok = true;
    }
}

impl Drop for ReembedGuard {
    fn drop(&mut self) {
        let mut m = lock_map(&self.map);
        if self.ok {
            m.remove(&self.key);
        } else if let Some(st) = m.get_mut(&self.key) {
            st.running = false;
            st.fails = st.fails.saturating_add(1);
            let shift = (st.fails - 1).min(20);
            let wait = self
                .base_ms
                .saturating_mul(1i64 << shift)
                .min(REEMBED_BACKOFF_MAX_MS.max(self.base_ms));
            st.retry_after_ms = crate::db::now_ms().saturating_add(wait);
        }
    }
}

pub struct Handler {
    pools: DbPools,
    max_response_bytes: usize,
    default_dim: usize,
    cache: VecCache,
    /// `(caller, collection)` pairs whose vectors are being recomputed.
    reembedding: ReembedMap,
    reembed_backoff_base_ms: i64,
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
            reembedding: Arc::new(StdMutex::new(HashMap::new())),
            reembed_backoff_base_ms: REEMBED_BACKOFF_BASE_MS,
        }
    }

    /// Override the first re-embed retry delay (doubles per failure, 30 min cap).
    pub fn with_reembed_backoff_ms(mut self, base_ms: i64) -> Self {
        self.reembed_backoff_base_ms = base_ms;
        self
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
        let now = crate::db::now_ms();
        {
            let m = lock_map(&self.reembedding);
            if let Some(st) = m.get(&key) {
                if st.running || now < st.retry_after_ms {
                    return Err(reembedding_msg(collection, &cfg.model));
                }
            }
        }
        let stale = info
            .as_ref()
            .is_some_and(|(_, m)| !m.is_empty() && *m != cfg.model);
        if !stale {
            return Ok(());
        }
        let guard = {
            let mut m = lock_map(&self.reembedding);
            let st = m.entry(key.clone()).or_default();
            if st.running {
                None
            } else {
                st.running = true;
                Some(ReembedGuard {
                    map: self.reembedding.clone(),
                    key: key.clone(),
                    ok: false,
                    base_ms: self.reembed_backoff_base_ms,
                })
            }
        };
        if let Some(guard) = guard {
            let (pool, cache, rpc, cfg) = (pool.clone(), self.cache.clone(), rpc.clone(), cfg.clone());
            tokio::spawn(async move {
                let (caller, collection) = guard.key.clone();
                match reembed_collection(&pool, &cache, &caller, &collection, &rpc, &cfg).await {
                    Ok(n) => {
                        eprintln!(
                            "[vector-db] re-embedded {n} rows of {collection} for model {}",
                            cfg.model
                        );
                        guard.succeed();
                    }
                    Err(e) => {
                        eprintln!("[vector-db] re-embed of {collection} failed: {e}");
                        // guard dropped here: arms the retry backoff
                    }
                }
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
                    refuse_fake_for_real_collection(&collection, &info, emb.fake)?;
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
                    refuse_fake_for_real_collection(&collection, &info, emb.fake)?;
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
                    refuse_fake_for_real_collection(&collection, &info, emb.fake)?;
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

const SHADOW_DDL: &str = "create table if not exists vectors_reembed (\
    collection TEXT NOT NULL, id TEXT NOT NULL, text TEXT NOT NULL, vector BLOB NOT NULL, \
    PRIMARY KEY (collection, id))";

/// Recompute every vector of a collection from its `text` with the configured
/// model. New vectors go to a shadow table; only when every row has a vector
/// computed from its current text are they swapped in, together with
/// `collections.model`/`dim`, in one transaction (`VecCache::swap_reembedded`).
/// Any failure leaves the original vectors and model untouched and clears the
/// shadow. Rows without text cannot be re-embedded and are deleted at the
/// swap. Returns the number of rows swapped.
async fn reembed_collection(
    pool: &sqlx::SqlitePool,
    cache: &VecCache,
    caller: &str,
    collection: &str,
    rpc: &Rpc,
    cfg: &EmbedConfig,
) -> Result<usize, String> {
    sqlx::query(SHADOW_DDL)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    // A shadow left by a crashed earlier attempt is discarded, not resumed.
    let clear = || async {
        let _ = sqlx::query("delete from vectors_reembed where collection = ?1")
            .bind(collection)
            .execute(pool)
            .await;
    };
    clear().await;
    let result = reembed_inner(pool, cache, caller, collection, rpc, cfg).await;
    if result.is_err() {
        clear().await;
    }
    result
}

async fn reembed_inner(
    pool: &sqlx::SqlitePool,
    cache: &VecCache,
    caller: &str,
    collection: &str,
    rpc: &Rpc,
    cfg: &EmbedConfig,
) -> Result<usize, String> {
    // One request per page, persisted to the shadow before the next page.
    let page = cfg.batch.max(1) as i64;
    const MAX_PASSES: usize = 10;
    for _ in 0..MAX_PASSES {
        // Rows with no shadow yet, or whose text changed since it was embedded.
        loop {
            let rows = sqlx::query(
                "select v.id as id, v.text as text from vectors v left join vectors_reembed s \
                   on s.collection = v.collection and s.id = v.id \
                 where v.collection = ?1 and v.text <> '' and (s.id is null or s.text <> v.text) \
                 limit ?2",
            )
            .bind(collection)
            .bind(page)
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                break;
            }
            let ids: Vec<String> = rows.iter().map(|r| r.get("id")).collect();
            let texts: Vec<String> = rows.iter().map(|r| r.get("text")).collect();
            let vectors = embed_many(rpc, cfg, &texts).await?;
            if let Some(d) = vectors.first().map(|v| v.len()) {
                if vectors.iter().any(|v| v.len() != d) {
                    return Err("model returned mixed dimensions".to_string());
                }
            }
            let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
            for ((id, text), v) in ids.iter().zip(&texts).zip(&vectors) {
                sqlx::query(
                    "insert into vectors_reembed (collection, id, text, vector) values (?1, ?2, ?3, ?4) \
                     on conflict(collection, id) do update set text = excluded.text, vector = excluded.vector",
                )
                .bind(collection)
                .bind(id)
                .bind(text)
                .bind(codec::encode(v))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            }
            tx.commit().await.map_err(|e| e.to_string())?;
        }
        let dims: Vec<i64> = sqlx::query_scalar(
            "select distinct length(vector) / 4 from vectors_reembed where collection = ?1",
        )
        .bind(collection)
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
        if dims.len() > 1 {
            return Err("model returned mixed dimensions".to_string());
        }
        match cache
            .swap_reembedded(pool, caller, collection, &cfg.model)
            .await?
        {
            SwapOutcome::Done(n) => return Ok(n),
            SwapOutcome::Stale(_) => continue, // rows changed meanwhile: embed them, retry
        }
    }
    Err(format!(
        "collection {collection} kept changing during re-embed"
    ))
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

    /// Behaviour switches for the fake `ai`.
    #[derive(Clone, Default)]
    struct AiOpts {
        /// Reject array input like an old `ai`.
        reject_arrays: bool,
        /// Fail array input with this exact error text (e.g. an auth error).
        array_error: Option<String>,
        /// Fail every request with this error.
        fail_all: Option<String>,
        /// Fail every request after this many successful ones.
        fail_after: Option<usize>,
        /// Called with the 1-based request number before replying.
        hook: Option<Arc<dyn Fn(usize) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>>,
    }

    /// Stand-in for the `ai` plugin. `array_ok == false` mimics an old `ai`
    /// that only accepts a string `input`.
    fn spawn_fake_ai(array_ok: bool) -> FakeAi {
        spawn_ai(AiOpts {
            reject_arrays: !array_ok,
            ..Default::default()
        })
    }

    fn spawn_ai(opts: AiOpts) -> FakeAi {
        let (tx, mut rx) = mpsc::channel::<RpcCall>(16);
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let seen = calls.clone();
        tokio::spawn(async move {
            let mut ok_count = 0usize;
            while let Some(call) = rx.recv().await {
                let p: Value = serde_json::from_slice(&call.params_json).unwrap();
                let model = p["model"].as_str().unwrap().to_string();
                let dim = model_dim(&model);
                // like a real provider: not unit length
                let emb = |t: &str| -> Vec<f32> {
                    fake_embed(&format!("{model}:{t}"), dim)
                        .iter()
                        .map(|x| x * 2.5)
                        .collect()
                };
                let n = match &p["input"] {
                    Value::Array(items) => items.len(),
                    _ => 1,
                };
                seen.lock().unwrap().push(n);
                let n_calls = seen.lock().unwrap().len();
                if let Some(h) = &opts.hook {
                    h(n_calls).await;
                }
                let forced = opts
                    .fail_all
                    .clone()
                    .or_else(|| opts.fail_after.filter(|a| ok_count >= *a).map(|_| "ai.embedding failed: provider down".to_string()));
                let reply = if let Some(e) = forced {
                    Err(e)
                } else {
                    match &p["input"] {
                        Value::String(t) => {
                            Ok(json!({"embedding": emb(t), "dim": dim, "model": model, "usage": {}}))
                        }
                        Value::Array(items) => {
                            if let Some(e) = &opts.array_error {
                                Err(e.clone())
                            } else if !opts.reject_arrays {
                                let v: Vec<Vec<f32>> =
                                    items.iter().map(|t| emb(t.as_str().unwrap())).collect();
                                Ok(json!({"embeddings": v, "dim": dim, "model": model, "usage": {}}))
                            } else {
                                Err("ai.embedding failed: invalid JSON: invalid type: sequence, expected a string".to_string())
                            }
                        }
                        _ => Err("bad input".to_string()),
                    }
                };
                if reply.is_ok() {
                    ok_count += 1;
                }
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

    // ---- review fixes: fake vectors, atomic re-embed, backoff, array detection ----

    fn cfg_fake_fallback(model: &str) -> crate::config::EmbedConfig {
        let mut c = cfg_for(model);
        c.fallback = crate::config::EmbedFallback::Fake;
        c
    }

    async fn seed_collection(h: &Handler, ai: &FakeAi, model: &str, n: usize) {
        let docs: Vec<Value> = (0..n)
            .map(|i| {
                let words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
                json!({"id": format!("{}", i + 1), "text": words[i % 6]})
            })
            .collect();
        h.handle_with_rpc(
            "agent",
            "vec_upsert_batch",
            json!({"collection": "c", "docs": docs}).to_string().as_bytes(),
            Some(ai.rpc.clone()),
            Some(&cfg_for(model)),
        )
        .await
        .unwrap();
    }

    async fn count_rows(h: &Handler) -> i64 {
        let pool = h.pools.pool_for("agent").await.unwrap();
        sqlx::query_scalar("select count(*) from vectors where collection = 'c'")
            .fetch_one(&pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn fake_fallback_never_written_to_real_model_collection() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let good = spawn_fake_ai(true);
        seed_collection(&h, &good, "a", 2).await;
        let down = spawn_ai(AiOpts { fail_all: Some("ai.embedding failed: down".into()), ..Default::default() });
        let cfg = cfg_fake_fallback("a");

        let err = h
            .handle_with_rpc("agent", "vec_upsert", br#"{"collection":"c","id":"new","text":"x"}"#, Some(down.rpc.clone()), Some(&cfg))
            .await
            .unwrap_err();
        assert!(err.contains("refusing fake vectors"), "err was: {err}");
        let err = h
            .handle_with_rpc(
                "agent",
                "vec_upsert_batch",
                br#"{"collection":"c","docs":[{"id":"n1","text":"x"},{"id":"n2","text":"y"}]}"#,
                Some(down.rpc.clone()),
                Some(&cfg),
            )
            .await
            .unwrap_err();
        assert!(err.contains("refusing fake vectors"), "err was: {err}");
        assert_eq!(count_rows(&h).await, 2, "nothing persisted");
        let stats = h.handle("agent", "vec_stats", br#"{"collection":"c"}"#).await.unwrap();
        assert_eq!(stats["model"], "a");

        // a brand-new collection may use fake vectors, but is never stamped with a model
        h.handle_with_rpc("agent", "vec_upsert", br#"{"collection":"fresh","id":"1","text":"x"}"#, Some(down.rpc.clone()), Some(&cfg))
            .await
            .unwrap();
        let stats = h.handle("agent", "vec_stats", br#"{"collection":"fresh"}"#).await.unwrap();
        assert_eq!(stats["model"], "");
    }

    #[tokio::test]
    async fn fake_fallback_query_on_real_collection_errors() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let good = spawn_fake_ai(true);
        seed_collection(&h, &good, "a", 3).await;
        let down = spawn_ai(AiOpts { fail_all: Some("ai.embedding failed: down".into()), ..Default::default() });
        let cfg = cfg_fake_fallback("a");
        let err = h
            .handle_with_rpc("agent", "vec_query", br#"{"collection":"c","text":"alpha","top_k":2}"#, Some(down.rpc.clone()), Some(&cfg))
            .await
            .unwrap_err();
        assert!(err.contains("refusing fake vectors"), "err was: {err}");
        // an explicit vector needs no embedding and still works
        let ok = h
            .handle_with_rpc(
                "agent",
                "vec_query",
                json!({"collection":"c","vector":[1,0,0,0,0,0,0,0],"top_k":2}).to_string().as_bytes(),
                Some(down.rpc.clone()),
                Some(&cfg),
            )
            .await
            .unwrap();
        assert_eq!(ok["results"].as_array().unwrap().len(), 2);
    }

    async fn vectors_of(h: &Handler) -> Vec<(String, usize)> {
        let pool = h.pools.pool_for("agent").await.unwrap();
        let rows: Vec<(String, i64)> =
            sqlx::query_as("select id, length(vector) / 4 from vectors where collection = 'c' order by id")
                .fetch_all(&pool)
                .await
                .unwrap();
        rows.into_iter().map(|(i, d)| (i, d as usize)).collect()
    }

    async fn shadow_rows(h: &Handler) -> i64 {
        let pool = h.pools.pool_for("agent").await.unwrap();
        sqlx::query_scalar("select count(*) from vectors_reembed")
            .fetch_one(&pool)
            .await
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn reembed_failure_midway_leaves_collection_on_old_model() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let good = spawn_fake_ai(true);
        seed_collection(&h, &good, "a", 5).await;
        let before = h.handle("agent", "vec_get", br#"{"collection":"c","id":"3"}"#).await.unwrap();
        let pool = h.pools.pool_for("agent").await.unwrap();

        let mut cfg_b = cfg_for("b");
        cfg_b.batch = 2; // 5 rows -> 3 requests
        let flaky = spawn_ai(AiOpts { fail_after: Some(1), ..Default::default() });
        let err = reembed_collection(&pool, h.cache(), "agent", "c", &flaky.rpc, &cfg_b)
            .await
            .unwrap_err();
        assert!(err.contains("provider down"), "err was: {err}");
        let stats = h.handle("agent", "vec_stats", br#"{"collection":"c"}"#).await.unwrap();
        assert_eq!((stats["model"].as_str().unwrap(), stats["dim"].as_i64().unwrap()), ("a", 8));
        assert!(vectors_of(&h).await.iter().all(|(_, d)| *d == 8), "no row was touched");
        assert_eq!(h.handle("agent", "vec_get", br#"{"collection":"c","id":"3"}"#).await.unwrap(), before);
        assert_eq!(shadow_rows(&h).await, 0, "shadow cleared after failure");
        // and it still searches on the old model
        let q = h
            .handle_with_rpc("agent", "vec_query", br#"{"collection":"c","text":"gamma","top_k":1}"#, Some(good.rpc.clone()), Some(&cfg_for("a")))
            .await
            .unwrap();
        assert_eq!(q["results"][0]["id"], "3");
    }

    #[tokio::test]
    async fn reembed_swap_is_atomic_with_dim_change() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let ai = spawn_fake_ai(true);
        seed_collection(&h, &ai, "a", 6).await;
        // warm the cache with the old vectors
        h.handle("agent", "vec_query", br#"{"collection":"c","vector":[1,0,0,0,0,0,0,0],"top_k":1}"#).await.unwrap();
        assert!(h.cache().is_loaded("agent", "c").await);
        let pool = h.pools.pool_for("agent").await.unwrap();

        // Observer: dim recorded on the collection and the vectors' real
        // dimension must agree in every committed snapshot.
        let (obs_pool, stop) = (pool.clone(), Arc::new(AtomicBool::new(false)));
        let stop2 = stop.clone();
        let observer = tokio::spawn(async move {
            let mut seen_new = false;
            let mut snapshots = 0u32;
            while !stop2.load(Ordering::SeqCst) {
                let (dim, lo, hi): (i64, i64, i64) = sqlx::query_as(
                    "select (select dim from collections where name = 'c'), \
                            min(length(vector) / 4), max(length(vector) / 4) \
                     from vectors where collection = 'c'",
                )
                .fetch_one(&obs_pool)
                .await
                .unwrap();
                assert!(dim == lo && lo == hi, "torn state: dim={dim} vectors={lo}..{hi}");
                seen_new |= dim == 12;
                snapshots += 1;
                tokio::task::yield_now().await;
            }
            (seen_new, snapshots)
        });

        let mut cfg_b = cfg_for("b");
        cfg_b.batch = 2;
        let kept = reembed_collection(&pool, h.cache(), "agent", "c", &ai.rpc, &cfg_b).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        stop.store(true, Ordering::SeqCst);
        let (seen_new, snapshots) = observer.await.unwrap();
        assert!(snapshots > 0 && seen_new);

        assert_eq!(kept, 6);
        let stats = h.handle("agent", "vec_stats", br#"{"collection":"c"}"#).await.unwrap();
        assert_eq!((stats["model"].as_str().unwrap(), stats["dim"].as_i64().unwrap()), ("b", 12));
        assert!(vectors_of(&h).await.iter().all(|(_, d)| *d == 12));
        assert_eq!(shadow_rows(&h).await, 0);
        assert!(!h.cache().is_loaded("agent", "c").await, "stale cache dropped by the swap");
        let q = h
            .handle_with_rpc("agent", "vec_query", br#"{"collection":"c","text":"gamma","top_k":1}"#, Some(ai.rpc.clone()), Some(&cfg_b))
            .await
            .unwrap();
        assert_eq!(q["results"][0]["id"], "3");
    }

    #[tokio::test]
    async fn row_changed_during_reembed_gets_new_text_vector() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let good = spawn_fake_ai(true);
        seed_collection(&h, &good, "a", 3).await;
        let pool = h.pools.pool_for("agent").await.unwrap();

        // while the first embedding request is in flight, row 2 is rewritten
        let hook_pool = pool.clone();
        let hook: Arc<dyn Fn(usize) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync> =
            Arc::new(move |n| {
                let p = hook_pool.clone();
                Box::pin(async move {
                    if n == 1 {
                        sqlx::query("update vectors set text = 'changed text' where collection = 'c' and id = '2'")
                            .execute(&p)
                            .await
                            .unwrap();
                    }
                })
            });
        let ai = spawn_ai(AiOpts { hook: Some(hook), ..Default::default() });
        let cfg_b = cfg_for("b");
        reembed_collection(&pool, h.cache(), "agent", "c", &ai.rpc, &cfg_b).await.unwrap();

        let got = h.handle("agent", "vec_get", br#"{"collection":"c","id":"2"}"#).await.unwrap();
        let got: Vec<f32> = serde_json::from_value(got["vector"].clone()).unwrap();
        let mut want = fake_embed("b:changed text", 12);
        normalize(&mut want);
        for (a, b) in got.iter().zip(&want) {
            assert!((a - b).abs() < 1e-5, "row 2 must carry the vector of its NEW text");
        }
        let calls = ai.calls.lock().unwrap().clone();
        assert_eq!(calls, vec![3, 1], "second pass embeds only the changed row");
    }

    #[tokio::test]
    async fn failed_reembed_backs_off_instead_of_respawning() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path()).with_reembed_backoff_ms(100_000);
        let good = spawn_fake_ai(true);
        seed_collection(&h, &good, "a", 3).await;
        let bad = spawn_ai(AiOpts { fail_all: Some("ai.embedding failed: boom".into()), ..Default::default() });
        let cfg_b = cfg_for("b");
        let q = br#"{"collection":"c","vector":[1,0,0,0,0,0,0,0],"top_k":1}"#;
        let key = ("agent".to_string(), "c".to_string());
        let ask = || h.handle_with_rpc("agent", "vec_query", q, Some(bad.rpc.clone()), Some(&cfg_b));

        assert!(ask().await.unwrap_err().contains("is being re-embedded"));
        // wait for the failing attempt to finish
        for _ in 0..100 {
            if lock_map(&h.reembedding).get(&key).is_some_and(|s| !s.running && s.fails == 1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let st = lock_map(&h.reembedding).get(&key).cloned().unwrap();
        assert!(!st.running && st.fails == 1, "key must not stay stuck: {st:?}");
        let attempts = bad.calls.lock().unwrap().len();
        assert!(attempts >= 1);

        // storm of requests during the backoff: same error, no new attempts
        for _ in 0..10 {
            assert!(ask().await.unwrap_err().contains("is being re-embedded for model b"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(bad.calls.lock().unwrap().len(), attempts, "no respawn during backoff");

        // backoff over -> one new attempt, backoff doubles
        lock_map(&h.reembedding).get_mut(&key).unwrap().retry_after_ms = 0;
        assert!(ask().await.is_err());
        for _ in 0..100 {
            if lock_map(&h.reembedding).get(&key).is_some_and(|s| !s.running && s.fails == 2) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let st = lock_map(&h.reembedding).get(&key).cloned().unwrap();
        assert_eq!(st.fails, 2);
        let wait = st.retry_after_ms - crate::db::now_ms();
        assert!(wait > 150_000 && wait <= 200_000, "doubled backoff, got {wait} ms");
        assert!(bad.calls.lock().unwrap().len() > attempts);
    }

    #[tokio::test]
    async fn reembed_guard_releases_key_on_panic() {
        let map: ReembedMap = Arc::new(StdMutex::new(HashMap::new()));
        let key = ("agent".to_string(), "c".to_string());
        lock_map(&map).insert(key.clone(), ReembedState { running: true, ..Default::default() });
        let guard = ReembedGuard { map: map.clone(), key: key.clone(), ok: false, base_ms: 1000 };
        let joined = tokio::spawn(async move {
            let _g = guard;
            panic!("re-embed task blew up");
        })
        .await;
        assert!(joined.is_err());
        let st = lock_map(&map).get(&key).cloned().unwrap();
        assert!(!st.running, "panic must not leave the collection stuck as running");
        assert_eq!(st.fails, 1);
        assert!(st.retry_after_ms > crate::db::now_ms());
    }

    #[tokio::test]
    async fn auth_error_on_array_does_not_flip_unsupported_flag() {
        let dir = tempfile::tempdir().unwrap();
        let h = make_handler(dir.path());
        let ai = spawn_ai(AiOpts { array_error: Some("ai.embedding failed: invalid_api_key".into()), ..Default::default() });
        let cfg = cfg_for("a");
        let batch = br#"{"collection":"c","docs":[{"id":"1","text":"a"},{"id":"2","text":"b"},{"id":"3","text":"c"}]}"#;
        for _ in 0..2 {
            let err = h
                .handle_with_rpc("agent", "vec_upsert_batch", batch, Some(ai.rpc.clone()), Some(&cfg))
                .await
                .unwrap_err();
            assert!(err.contains("invalid_api_key"), "err was: {err}");
        }
        assert!(!ai.rpc.array_unsupported.load(Ordering::Relaxed), "auth error must not flip the flag");
        assert_eq!(*ai.calls.lock().unwrap(), vec![3, 3], "array still tried both times, never degraded to singles");
        assert!(looks_like_array_unsupported("ai.embedding failed: invalid JSON: invalid type: sequence, expected a string"));
        assert!(!looks_like_array_unsupported("provider returned HTTP 401: invalid_api_key"));
    }
}
