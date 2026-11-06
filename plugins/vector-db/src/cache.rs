//! Lazy in-memory cache of whole collections for fast cosine search.
//!
//! A collection is loaded on first query into one contiguous `Vec<f32>` (plus
//! ids and filter metadata). Writes are write-through: the SQLite write and
//! the cache update happen together under a per-collection writer mutex, so a
//! reader never sees a half-applied write and a load never loses a concurrent
//! one. Collections idle for `idle_secs` are unloaded; the sum of loaded
//! collections is kept under `max_bytes` by evicting the least recently used,
//! and a collection that alone exceeds the ceiling is never cached (queries
//! scan BLOBs from SQLite in pages instead).

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Map, Value};
use sqlx::{Row, SqlitePool};
use tokio::sync::{Mutex, RwLock};

use crate::codec;

pub type Meta = Map<String, Value>;

const PAGE_ROWS: i64 = 2048;
/// Rows scanned inline (no spawn_blocking) when the collection is small.
const INLINE_SCAN_FLOATS: usize = 200_000;

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub max_bytes: usize,
    pub idle_ms: i64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024 * 1024,
            idle_ms: 600 * 1000,
        }
    }
}

/// One row to write (vector already normalized and dimension-checked).
#[derive(Debug, Clone)]
pub struct UpsertRow {
    pub id: String,
    pub text: String,
    pub vector: Vec<f32>,
    pub metadata_json: String,
}

struct Loaded {
    dim: usize,
    ids: Vec<String>,
    data: Vec<f32>,
    meta: Vec<Meta>,
    index: HashMap<String, usize>,
    /// Sum of per-row overhead (id + metadata estimate), for byte accounting.
    overhead: usize,
}

fn row_overhead(id_len: usize, meta_len: usize) -> usize {
    id_len + 2 * meta_len + 112
}

impl Loaded {
    fn new(dim: usize) -> Self {
        Self {
            dim,
            ids: Vec::new(),
            data: Vec::new(),
            meta: Vec::new(),
            index: HashMap::new(),
            overhead: 0,
        }
    }

    fn bytes(&self) -> usize {
        self.data.len() * 4 + self.overhead
    }

    fn push(&mut self, id: String, vector: &[f32], meta_json: &str) {
        let meta = parse_meta(meta_json);
        if let Some(&i) = self.index.get(&id) {
            self.data[i * self.dim..(i + 1) * self.dim].copy_from_slice(vector);
            self.overhead -= row_overhead(self.ids[i].len(), meta_len_of(&self.meta[i]));
            self.overhead += row_overhead(id.len(), meta_json.len());
            self.meta[i] = meta;
        } else {
            self.index.insert(id.clone(), self.ids.len());
            self.overhead += row_overhead(id.len(), meta_json.len());
            self.ids.push(id);
            self.data.extend_from_slice(vector);
            self.meta.push(meta);
        }
    }

    fn remove(&mut self, id: &str) -> bool {
        let Some(i) = self.index.remove(id) else {
            return false;
        };
        let last = self.ids.len() - 1;
        self.overhead -= row_overhead(self.ids[i].len(), meta_len_of(&self.meta[i]));
        if i != last {
            let (head, tail) = self.data.split_at_mut(last * self.dim);
            head[i * self.dim..(i + 1) * self.dim].copy_from_slice(&tail[..self.dim]);
            self.ids.swap(i, last);
            self.meta.swap(i, last);
            let moved = self.ids[i].clone();
            self.index.insert(moved, i);
        }
        self.ids.pop();
        self.meta.pop();
        self.data.truncate(last * self.dim);
        true
    }
}

fn meta_len_of(m: &Meta) -> usize {
    // Approximation used only for accounting symmetry on replace/remove.
    serde_json::to_string(m).map(|s| s.len()).unwrap_or(2)
}

fn parse_meta(json: &str) -> Meta {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::Object(m)) => m,
        _ => Meta::new(),
    }
}

pub fn matches_filter(meta: &Meta, filter: &Meta) -> bool {
    filter.iter().all(|(k, v)| meta.get(k) == Some(v))
}

#[derive(Clone, Copy)]
struct Cand {
    score: f32,
    idx: usize,
}

impl PartialEq for Cand {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Cand {
    /// "Greater" = better: higher score; on ties the earlier row wins.
    fn cmp(&self, o: &Self) -> Ordering {
        self.score
            .total_cmp(&o.score)
            .then_with(|| o.idx.cmp(&self.idx))
    }
}

/// Bounded best-k selector (min-heap of the current best k).
struct TopK {
    k: usize,
    heap: BinaryHeap<Reverse<Cand>>,
}

impl TopK {
    fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k + 1),
        }
    }
    /// Returns whether the candidate entered the best-k set.
    fn offer(&mut self, c: Cand) -> bool {
        if self.heap.len() < self.k {
            self.heap.push(Reverse(c));
            true
        } else if let Some(Reverse(worst)) = self.heap.peek() {
            if c > *worst {
                self.heap.pop();
                self.heap.push(Reverse(c));
                true
            } else {
                false
            }
        } else {
            false
        }
    }
    /// Best first.
    fn into_sorted(self) -> Vec<Cand> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse(c)| c)
            .collect()
    }
}

#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let ca = a.chunks_exact(8);
    let cb = b.chunks_exact(8);
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, y) in ca.zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

struct Slot {
    state: Arc<RwLock<Option<Loaded>>>,
    /// Serializes writers and loaders of this collection.
    writer: Mutex<()>,
    last_used: AtomicI64,
    bytes: AtomicUsize,
}

struct Inner {
    cfg: CacheConfig,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    slots: StdMutex<HashMap<(String, String), Arc<Slot>>>,
    /// Bytes held (or reserved for an in-flight load) by all cached collections.
    total: AtomicUsize,
}

#[derive(Clone)]
pub struct VecCache {
    inner: Arc<Inner>,
}

impl VecCache {
    pub fn new(cfg: CacheConfig) -> Self {
        Self::with_clock(cfg, Arc::new(crate::db::now_ms))
    }

    pub fn with_clock(cfg: CacheConfig, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                clock,
                slots: StdMutex::new(HashMap::new()),
                total: AtomicUsize::new(0),
            }),
        }
    }

    fn now(&self) -> i64 {
        (self.inner.clock)()
    }

    fn slot(&self, caller: &str, collection: &str) -> Arc<Slot> {
        let mut g = self.inner.slots.lock().unwrap();
        g.entry((caller.to_string(), collection.to_string()))
            .or_insert_with(|| {
                Arc::new(Slot {
                    state: Arc::new(RwLock::new(None)),
                    writer: Mutex::new(()),
                    last_used: AtomicI64::new(0),
                    bytes: AtomicUsize::new(0),
                })
            })
            .clone()
    }

    fn total_bytes(&self) -> usize {
        self.inner.total.load(AtomicOrdering::SeqCst)
    }

    /// Atomically claim `n` bytes of the global budget; false if it would exceed the cap.
    fn try_reserve(&self, n: usize) -> bool {
        let max = self.inner.cfg.max_bytes;
        self.inner
            .total
            .fetch_update(AtomicOrdering::SeqCst, AtomicOrdering::SeqCst, |t| {
                t.checked_add(n).filter(|v| *v <= max)
            })
            .is_ok()
    }

    /// Reserve `n` bytes, evicting least recently used collections if needed.
    fn reserve_with_eviction(&self, keep: &Arc<Slot>, n: usize) -> bool {
        if self.try_reserve(n) {
            return true;
        }
        self.make_room(keep, n);
        self.try_reserve(n)
    }

    fn release(&self, n: usize) {
        // Saturating: the counter must never wrap even if accounting drifts.
        let _ = self
            .inner
            .total
            .fetch_update(AtomicOrdering::SeqCst, AtomicOrdering::SeqCst, |t| {
                Some(t.saturating_sub(n))
            });
    }

    /// Re-account a loaded slot whose size changed to `new` bytes (caller holds
    /// the slot's state write guard). Growth is reserved globally; false means
    /// no room, and the caller must unload the slot.
    fn account(&self, slot: &Arc<Slot>, new: usize) -> bool {
        let old = slot.bytes.load(AtomicOrdering::SeqCst);
        if new > old {
            if !self.reserve_with_eviction(slot, new - old) {
                return false;
            }
        } else {
            self.release(old - new);
        }
        slot.bytes.store(new, AtomicOrdering::SeqCst);
        true
    }

    /// Return a slot's accounted bytes to the global budget (call right after
    /// its state was set to `None`).
    fn drop_bytes(&self, slot: &Slot) {
        let b = slot.bytes.swap(0, AtomicOrdering::SeqCst);
        self.release(b);
    }

    /// Sum of what loaded collections actually hold (independent of the budget counter).
    #[cfg(test)]
    fn held_bytes(&self) -> usize {
        let g = self.inner.slots.lock().unwrap();
        g.values().map(|s| s.bytes.load(AtomicOrdering::SeqCst)).sum()
    }

    /// True when the collection is currently held in memory.
    pub async fn is_loaded(&self, caller: &str, collection: &str) -> bool {
        let slot = self.slot(caller, collection);
        let loaded = slot.state.read().await.is_some();
        loaded
    }

    /// Drop a collection from memory (e.g. after a re-embed rewrote it).
    pub async fn invalidate(&self, caller: &str, collection: &str) {
        let slot = self.slot(caller, collection);
        let _w = slot.writer.lock().await;
        let mut st = slot.state.write().await;
        if st.take().is_some() {
            self.drop_bytes(&slot);
        }
    }

    /// Unload collections not touched for longer than the idle window.
    /// Returns how many were unloaded.
    pub fn evict_idle(&self, now: i64) -> usize {
        let slots: Vec<Arc<Slot>> = self.inner.slots.lock().unwrap().values().cloned().collect();
        let mut n = 0;
        for s in slots {
            if s.bytes.load(AtomicOrdering::Relaxed) == 0 {
                continue;
            }
            if now - s.last_used.load(AtomicOrdering::Relaxed) > self.inner.cfg.idle_ms {
                if let Ok(mut st) = s.state.try_write() {
                    if st.take().is_some() {
                        self.drop_bytes(&s);
                        n += 1;
                    }
                }
            }
        }
        n
    }

    /// Estimated in-memory size of a collection, from SQLite.
    async fn estimate(pool: &SqlitePool, collection: &str, dim: usize) -> Result<usize, String> {
        let row = sqlx::query(
            "select count(*) as n, coalesce(sum(length(id) + 2 * length(metadata) + 112), 0) as oh \
             from vectors where collection = ?1",
        )
        .bind(collection)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
        let n: i64 = row.get("n");
        let oh: i64 = row.get("oh");
        Ok(n as usize * dim * 4 + oh as usize)
    }

    async fn collection_dim(pool: &SqlitePool, collection: &str) -> Result<Option<usize>, String> {
        let d: Option<i64> = sqlx::query_scalar("select dim from collections where name = ?1")
            .bind(collection)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(d.map(|d| d as usize))
    }

    /// Make room for `need` bytes by unloading least-recently-used collections.
    fn make_room(&self, keep: &Arc<Slot>, need: usize) {
        loop {
            if self.total_bytes().saturating_add(need) <= self.inner.cfg.max_bytes {
                return;
            }
            let victim = {
                let g = self.inner.slots.lock().unwrap();
                g.values()
                    .filter(|s| !Arc::ptr_eq(s, keep) && s.bytes.load(AtomicOrdering::Relaxed) > 0)
                    .min_by_key(|s| s.last_used.load(AtomicOrdering::Relaxed))
                    .cloned()
            };
            let Some(v) = victim else { return };
            match v.state.try_write() {
                Ok(mut st) => {
                    if st.take().is_some() {
                        self.drop_bytes(&v);
                    }
                }
                // In use right now: treat as not evictable this round.
                Err(_) => return,
            };
        }
    }

    async fn load(
        &self,
        pool: &SqlitePool,
        collection: &str,
        dim: usize,
    ) -> Result<Loaded, String> {
        let mut l = Loaded::new(dim);
        let mut last: i64 = 0;
        loop {
            let rows = sqlx::query(
                "select rowid, id, vector, metadata from vectors \
                 where collection = ?1 and rowid > ?2 order by rowid limit ?3",
            )
            .bind(collection)
            .bind(last)
            .bind(PAGE_ROWS)
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                break;
            }
            for r in rows {
                last = r.get::<i64, _>("rowid");
                let id: String = r.get("id");
                let blob: Vec<u8> = r.get("vector");
                match codec::decode(&blob) {
                    Ok(v) if v.len() == dim => {
                        let m: String = r.get("metadata");
                        l.push(id, &v, &m);
                    }
                    _ => eprintln!(
                        "[vector-db] warn: skipping vector with bad length: collection={collection} id={id}"
                    ),
                }
            }
        }
        Ok(l)
    }

    /// Top-k `(id, cosine)` for a normalized query vector, best first.
    pub async fn query(
        &self,
        pool: &SqlitePool,
        caller: &str,
        collection: &str,
        q: &[f32],
        top_k: usize,
        filter: Option<&Meta>,
    ) -> Result<Vec<(String, f32)>, String> {
        let slot = self.slot(caller, collection);
        slot.last_used.store(self.now(), AtomicOrdering::Relaxed);

        loop {
            let g = slot.state.clone().read_owned().await;
            if let Some(l) = g.as_ref() {
                if l.dim != q.len() {
                    return Err(format!(
                        "dimension mismatch: collection '{collection}' dim {}, got {}",
                        l.dim,
                        q.len()
                    ));
                }
                if l.data.len() > INLINE_SCAN_FLOATS {
                    let q = q.to_vec();
                    let f = filter.cloned();
                    return tokio::task::spawn_blocking(move || {
                        scan_loaded(g.as_ref().unwrap(), &q, top_k, f.as_ref())
                    })
                    .await
                    .map_err(|e| e.to_string());
                }
                return Ok(scan_loaded(l, q, top_k, filter));
            }
            drop(g);

            // Not loaded: become the (single) loader, or notice someone loaded it.
            let _w = slot.writer.lock().await;
            if slot.state.read().await.is_some() {
                continue;
            }
            let Some(dim) = Self::collection_dim(pool, collection).await? else {
                return Ok(Vec::new());
            };
            if dim != q.len() {
                return Err(format!(
                    "dimension mismatch: collection '{collection}' dim {dim}, got {}",
                    q.len()
                ));
            }
            let est = Self::estimate(pool, collection, dim).await?;
            if est > self.inner.cfg.max_bytes {
                drop(_w);
                return scan_sqlite(pool, collection, q, top_k, filter).await;
            }
            // Reserve before loading so concurrent loads cannot jointly overcommit.
            if !self.reserve_with_eviction(&slot, est) {
                drop(_w);
                return scan_sqlite(pool, collection, q, top_k, filter).await;
            }
            let loaded = match self.load(pool, collection, dim).await {
                Ok(l) => l,
                Err(e) => {
                    self.release(est);
                    return Err(e);
                }
            };
            let actual = loaded.bytes();
            if actual > est {
                // The estimate was low (rows grew meanwhile); claim the rest or give up.
                if !self.reserve_with_eviction(&slot, actual - est) {
                    self.release(est);
                    drop(_w);
                    return scan_sqlite(pool, collection, q, top_k, filter).await;
                }
            } else {
                self.release(est - actual);
            }
            // Install state and bytes together under the write guard so make_room
            // never sees one without the other.
            let mut st = slot.state.write().await;
            slot.bytes.store(actual, AtomicOrdering::SeqCst);
            *st = Some(loaded);
        }
    }

    /// Write-through upsert: one SQLite transaction (creating the collection
    /// if needed), then the in-memory update, under the writer mutex.
    /// `model` (non-empty) is recorded on the collection when it has none.
    pub async fn upsert(
        &self,
        pool: &SqlitePool,
        caller: &str,
        collection: &str,
        dim: usize,
        model: Option<&str>,
        rows: Vec<UpsertRow>,
    ) -> Result<usize, String> {
        let slot = self.slot(caller, collection);
        let _w = slot.writer.lock().await;
        let now = crate::db::now_ms();
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        sqlx::query(
            "insert or ignore into collections (name, dim, created_at) values (?1, ?2, ?3)",
        )
        .bind(collection)
        .bind(dim as i64)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        let existing: i64 = sqlx::query_scalar("select dim from collections where name = ?1")
            .bind(collection)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        if existing as usize != dim {
            return Err(format!(
                "dimension mismatch: collection '{collection}' dim {existing}, got {dim}"
            ));
        }
        if let Some(m) = model.filter(|m| !m.is_empty()) {
            sqlx::query("update collections set model = ?2 where name = ?1 and model = ''")
                .bind(collection)
                .bind(m)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        for r in &rows {
            sqlx::query(
                "insert into vectors (collection, id, text, vector, metadata, updated_at) \
                 values (?1, ?2, ?3, ?4, ?5, ?6) \
                 on conflict(collection, id) do update set text=excluded.text, \
                 vector=excluded.vector, metadata=excluded.metadata, updated_at=excluded.updated_at",
            )
            .bind(collection)
            .bind(&r.id)
            .bind(&r.text)
            .bind(codec::encode(&r.vector))
            .bind(&r.metadata_json)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())?;

        let n = rows.len();
        let mut st = slot.state.write().await;
        if let Some(l) = st.as_mut() {
            for r in rows {
                l.push(r.id, &r.vector, &r.metadata_json);
            }
            let new = l.bytes();
            if !self.account(&slot, new) {
                // No room: unload so later queries scan SQLite.
                *st = None;
                self.drop_bytes(&slot);
            }
        }
        slot.last_used.store(self.now(), AtomicOrdering::Relaxed);
        Ok(n)
    }

    /// Write-through delete. Returns whether a row existed.
    pub async fn delete(
        &self,
        pool: &SqlitePool,
        caller: &str,
        collection: &str,
        id: &str,
    ) -> Result<bool, String> {
        let slot = self.slot(caller, collection);
        let _w = slot.writer.lock().await;
        let res = sqlx::query("delete from vectors where collection = ?1 and id = ?2")
            .bind(collection)
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        let mut st = slot.state.write().await;
        if let Some(l) = st.as_mut() {
            l.remove(id);
            let new = l.bytes();
            if !self.account(&slot, new) {
                *st = None;
                self.drop_bytes(&slot);
            }
        }
        Ok(res.rows_affected() > 0)
    }
}

fn scan_loaded(l: &Loaded, q: &[f32], top_k: usize, filter: Option<&Meta>) -> Vec<(String, f32)> {
    let mut top = TopK::new(top_k);
    for (i, row) in l.data.chunks_exact(l.dim).enumerate() {
        if let Some(f) = filter {
            if !matches_filter(&l.meta[i], f) {
                continue;
            }
        }
        top.offer(Cand {
            score: dot(q, row),
            idx: i,
        });
    }
    top.into_sorted()
        .into_iter()
        .map(|c| (l.ids[c.idx].clone(), c.score))
        .collect()
}

/// Uncached path: page through BLOBs in SQLite, keeping only the best k.
async fn scan_sqlite(
    pool: &SqlitePool,
    collection: &str,
    q: &[f32],
    top_k: usize,
    filter: Option<&Meta>,
) -> Result<Vec<(String, f32)>, String> {
    let mut top = TopK::new(top_k);
    let mut ids: HashMap<usize, String> = HashMap::new();
    let mut last: i64 = 0;
    loop {
        let rows = sqlx::query(
            "select rowid, id, vector, metadata from vectors \
             where collection = ?1 and rowid > ?2 order by rowid limit ?3",
        )
        .bind(collection)
        .bind(last)
        .bind(PAGE_ROWS)
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
        if rows.is_empty() {
            break;
        }
        for r in rows {
            let rowid: i64 = r.get("rowid");
            last = rowid;
            if let Some(f) = filter {
                let m: String = r.get("metadata");
                if !matches_filter(&parse_meta(&m), f) {
                    continue;
                }
            }
            let blob: Vec<u8> = r.get("vector");
            let Ok(v) = codec::decode(&blob) else {
                continue;
            };
            if v.len() != q.len() {
                continue;
            }
            let c = Cand {
                score: dot(q, &v),
                idx: rowid as usize,
            };
            if top.offer(c) {
                ids.insert(c.idx, r.get("id"));
            }
        }
    }
    Ok(top
        .into_sorted()
        .into_iter()
        .filter_map(|c| ids.get(&c.idx).map(|id| (id.clone(), c.score)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{DbConfig, DbPools};
    use std::sync::atomic::AtomicI64;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn unit(&mut self, dim: usize) -> Vec<f32> {
            let mut v: Vec<f32> = (0..dim)
                .map(|_| (self.next() % 20001) as f32 / 10000.0 - 1.0)
                .collect();
            crate::embed::normalize(&mut v);
            v
        }
    }

    fn rows(rng: &mut Rng, n: usize, dim: usize, prefix: &str) -> Vec<UpsertRow> {
        (0..n)
            .map(|i| UpsertRow {
                id: format!("{prefix}{i}"),
                text: format!("text {i}"),
                vector: rng.unit(dim),
                metadata_json: format!(
                    "{{\"parity\":\"{}\"}}",
                    if i % 2 == 0 { "even" } else { "odd" }
                ),
            })
            .collect()
    }

    async fn setup() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pools = DbPools::new(DbConfig {
            data_dir: dir.path().to_path_buf(),
            pool_size: 4,
            busy_timeout_ms: 5000,
            max_db_bytes: 0,
        });
        let pool = pools.pool_for("t").await.unwrap();
        (dir, pool)
    }

    /// At quiescence the global counter must equal what loaded slots hold.
    fn assert_accounting(cache: &VecCache) {
        assert_eq!(cache.total_bytes(), cache.held_bytes(), "global total drifted from slot bytes");
        // every slot's accounted bytes match what it really holds
        let slots: Vec<Arc<Slot>> = cache.inner.slots.lock().unwrap().values().cloned().collect();
        for sl in slots {
            let st = sl.state.try_read().expect("quiescent");
            let want = st.as_ref().map(|l| l.bytes()).unwrap_or(0);
            assert_eq!(sl.bytes.load(AtomicOrdering::SeqCst), want, "slot bytes out of sync with its state");
        }
    }

    fn cfg(max_mb_bytes: usize) -> CacheConfig {
        CacheConfig {
            max_bytes: max_mb_bytes,
            idle_ms: 1000,
        }
    }

    #[tokio::test]
    async fn cached_query_matches_sql_scan() {
        let (_d, pool) = setup().await;
        let cache = VecCache::new(CacheConfig::default());
        let mut rng = Rng(0x9E3779B97F4A7C15);
        cache
            .upsert(&pool, "t", "c", 64, None, rows(&mut rng, 500, 64, "d"))
            .await
            .unwrap();
        for _ in 0..20 {
            let q = rng.unit(64);
            let via_cache = cache.query(&pool, "t", "c", &q, 10, None).await.unwrap();
            let via_sql = scan_sqlite(&pool, "c", &q, 10, None).await.unwrap();
            assert_eq!(via_cache.len(), 10);
            assert!(cache.is_loaded("t", "c").await);
            assert_eq!(via_cache.len(), via_sql.len());
            for (a, b) in via_cache.iter().zip(&via_sql) {
                assert_eq!(a.0, b.0);
                assert!((a.1 - b.1).abs() < 1e-6);
            }
            // best-first order
            assert!(via_cache.windows(2).all(|w| w[0].1 >= w[1].1));
        }
    }

    #[tokio::test]
    async fn upsert_visible_in_cache() {
        let (_d, pool) = setup().await;
        let cache = VecCache::new(CacheConfig::default());
        let mut rng = Rng(7);
        cache
            .upsert(&pool, "t", "c", 16, None, rows(&mut rng, 10, 16, "d"))
            .await
            .unwrap();
        let probe = rng.unit(16);
        // warm: loaded now
        cache.query(&pool, "t", "c", &probe, 3, None).await.unwrap();
        assert!(cache.is_loaded("t", "c").await);
        // add after load -> visible without reload
        let new = UpsertRow {
            id: "fresh".into(),
            text: "x".into(),
            vector: probe.clone(),
            metadata_json: "{}".into(),
        };
        cache
            .upsert(&pool, "t", "c", 16, None, vec![new])
            .await
            .unwrap();
        let hit = cache.query(&pool, "t", "c", &probe, 1, None).await.unwrap();
        assert_eq!(hit[0].0, "fresh");
        assert!((hit[0].1 - 1.0).abs() < 1e-5);
        // replace an existing id with a different vector
        let other = rng.unit(16);
        let rep = UpsertRow {
            id: "fresh".into(),
            text: "x".into(),
            vector: other.clone(),
            metadata_json: "{}".into(),
        };
        cache
            .upsert(&pool, "t", "c", 16, None, vec![rep])
            .await
            .unwrap();
        let hit = cache.query(&pool, "t", "c", &other, 1, None).await.unwrap();
        assert_eq!(hit[0].0, "fresh");
        let hit = cache
            .query(&pool, "t", "c", &probe, 20, None)
            .await
            .unwrap();
        assert_eq!(
            hit.iter().filter(|h| h.0 == "fresh").count(),
            1,
            "no duplicate row"
        );
        assert_eq!(hit.len(), 11);
    }

    #[tokio::test]
    async fn delete_removed_from_cache() {
        let (_d, pool) = setup().await;
        let cache = VecCache::new(CacheConfig::default());
        let mut rng = Rng(11);
        let rs = rows(&mut rng, 10, 16, "d");
        let target = rs[3].vector.clone();
        cache.upsert(&pool, "t", "c", 16, None, rs).await.unwrap();
        assert_eq!(
            cache
                .query(&pool, "t", "c", &target, 1, None)
                .await
                .unwrap()[0]
                .0,
            "d3"
        );
        assert!(cache.delete(&pool, "t", "c", "d3").await.unwrap());
        assert!(!cache.delete(&pool, "t", "c", "d3").await.unwrap());
        let all = cache
            .query(&pool, "t", "c", &target, 20, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 9);
        assert!(all.iter().all(|h| h.0 != "d3"));
        assert_accounting(&cache);
        // swap-remove kept the other ids addressable with their own vectors
        for i in [0usize, 9, 5] {
            let v = scan_sqlite(&pool, "c", &target, 20, None).await.unwrap();
            assert!(v.iter().any(|h| h.0 == format!("d{i}")));
        }
        let via_cache = cache
            .query(&pool, "t", "c", &target, 20, None)
            .await
            .unwrap();
        let via_sql = scan_sqlite(&pool, "c", &target, 20, None).await.unwrap();
        assert_eq!(
            via_cache.iter().map(|h| &h.0).collect::<Vec<_>>(),
            via_sql.iter().map(|h| &h.0).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn filter_applies_in_cache() {
        let (_d, pool) = setup().await;
        let cache = VecCache::new(CacheConfig::default());
        let mut rng = Rng(5);
        cache
            .upsert(&pool, "t", "c", 16, None, rows(&mut rng, 20, 16, "d"))
            .await
            .unwrap();
        let q = rng.unit(16);
        let mut f = Meta::new();
        f.insert("parity".into(), Value::String("odd".into()));
        let hits = cache
            .query(&pool, "t", "c", &q, 50, Some(&f))
            .await
            .unwrap();
        assert_eq!(hits.len(), 10);
        assert!(hits
            .iter()
            .all(|h| h.0[1..].parse::<usize>().unwrap() % 2 == 1));
        assert!(
            cache.is_loaded("t", "c").await,
            "filtered query used the cache"
        );
    }

    #[tokio::test]
    async fn idle_collection_unloaded() {
        let (_d, pool) = setup().await;
        let now = Arc::new(AtomicI64::new(1_000_000));
        let n2 = now.clone();
        let cache = VecCache::with_clock(
            cfg(1 << 30),
            Arc::new(move || n2.load(AtomicOrdering::SeqCst)),
        );
        let mut rng = Rng(3);
        cache
            .upsert(&pool, "t", "c", 8, None, rows(&mut rng, 5, 8, "d"))
            .await
            .unwrap();
        let q = rng.unit(8);
        cache.query(&pool, "t", "c", &q, 1, None).await.unwrap();
        assert!(cache.is_loaded("t", "c").await);
        assert_eq!(
            cache.evict_idle(now.load(AtomicOrdering::SeqCst) + 500),
            0,
            "not idle yet"
        );
        assert!(cache.is_loaded("t", "c").await);
        now.fetch_add(5000, AtomicOrdering::SeqCst);
        assert_eq!(cache.evict_idle(now.load(AtomicOrdering::SeqCst)), 1);
        assert!(!cache.is_loaded("t", "c").await);
        assert_eq!(cache.total_bytes(), 0);
        // transparently reloads
        assert_eq!(
            cache
                .query(&pool, "t", "c", &q, 5, None)
                .await
                .unwrap()
                .len(),
            5
        );
        assert!(cache.is_loaded("t", "c").await);
    }

    #[tokio::test]
    async fn over_ceiling_falls_back_to_scan() {
        let (_d, pool) = setup().await;
        // 600 x 1024 x 4 B = 2.4 MB collection vs 1 MiB ceiling
        let cache = VecCache::new(cfg(1024 * 1024));
        let mut rng = Rng(99);
        let rs = rows(&mut rng, 600, 1024, "big");
        let target = rs[417].vector.clone();
        cache
            .upsert(&pool, "t", "big", 1024, None, rs)
            .await
            .unwrap();
        let hit = cache
            .query(&pool, "t", "big", &target, 3, None)
            .await
            .unwrap();
        assert_eq!(hit[0].0, "big417");
        assert!((hit[0].1 - 1.0).abs() < 1e-4);
        assert!(
            !cache.is_loaded("t", "big").await,
            "over-ceiling collection must not be cached"
        );
        assert_eq!(cache.total_bytes(), 0);
        // a small collection still caches next to it
        cache
            .upsert(&pool, "t", "small", 8, None, rows(&mut rng, 4, 8, "s"))
            .await
            .unwrap();
        cache
            .query(&pool, "t", "small", &rng.unit(8), 1, None)
            .await
            .unwrap();
        assert!(cache.is_loaded("t", "small").await);
    }

    #[tokio::test]
    async fn lru_eviction_keeps_total_under_ceiling() {
        let (_d, pool) = setup().await;
        let now = Arc::new(AtomicI64::new(1));
        let n2 = now.clone();
        // each collection: 100 x 256 x 4 = ~100 KB + overhead; ceiling fits two
        let cache = VecCache::with_clock(
            cfg(260_000),
            Arc::new(move || n2.fetch_add(1, AtomicOrdering::SeqCst)),
        );
        let mut rng = Rng(21);
        for c in ["a", "b", "c"] {
            cache
                .upsert(&pool, "t", c, 256, None, rows(&mut rng, 100, 256, c))
                .await
                .unwrap();
        }
        let q = rng.unit(256);
        for c in ["a", "b", "c"] {
            cache.query(&pool, "t", c, &q, 1, None).await.unwrap();
        }
        assert!(
            cache.total_bytes() <= 260_000,
            "total {}",
            cache.total_bytes()
        );
        assert!(
            !cache.is_loaded("t", "a").await,
            "least recently used evicted"
        );
        assert!(cache.is_loaded("t", "c").await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_upsert_and_query_consistent() {
        for round in 0..6 {
            concurrent_round(round).await;
        }
    }

    async fn concurrent_round(round: u64) {
        let (_d, pool) = setup().await;
        let cache = VecCache::new(CacheConfig::default());
        let dim = 16;
        // seed so the collection is loaded while writers race
        let mut seed = Rng(1);
        cache
            .upsert(&pool, "t", "c", dim, None, rows(&mut seed, 5, dim, "seed"))
            .await
            .unwrap();
        cache
            .query(&pool, "t", "c", &seed.unit(dim), 1, None)
            .await
            .unwrap();

        let mut writers = Vec::new();
        for w in 0..8u64 {
            let (cache, pool) = (cache.clone(), pool.clone());
            writers.push(tokio::spawn(async move {
                let mut rng = Rng(1000 + w + round * 100);
                let mut kept = Vec::new();
                for i in 0..25 {
                    let id = format!("w{w}-{i}");
                    let v = rng.unit(dim);
                    let row = UpsertRow {
                        id: id.clone(),
                        text: String::new(),
                        vector: v.clone(),
                        metadata_json: "{}".into(),
                    };
                    cache
                        .upsert(&pool, "t", "c", dim, None, vec![row])
                        .await
                        .unwrap();
                    if i % 5 == 4 {
                        assert!(cache.delete(&pool, "t", "c", &id).await.unwrap());
                    } else {
                        kept.push((id, v));
                    }
                }
                kept
            }));
        }
        let mut readers = Vec::new();
        for r in 0..8u64 {
            let (cache, pool) = (cache.clone(), pool.clone());
            readers.push(tokio::spawn(async move {
                let mut rng = Rng(5000 + r + round * 100);
                for _ in 0..40 {
                    let q = rng.unit(dim);
                    let hits = cache.query(&pool, "t", "c", &q, 5, None).await.unwrap();
                    assert!(hits.len() <= 5 && !hits.is_empty());
                    tokio::task::yield_now().await;
                }
            }));
        }
        let mut kept = Vec::new();
        for w in writers {
            kept.extend(w.await.unwrap());
        }
        for r in readers {
            r.await.unwrap();
        }
        assert_eq!(kept.len(), 8 * 20);
        for (id, v) in &kept {
            let hit = cache.query(&pool, "t", "c", v, 1, None).await.unwrap();
            assert_eq!(&hit[0].0, id, "written vector must be found by itself");
        }
        // deleted ones are gone from cache and db alike
        let n: i64 = sqlx::query_scalar("select count(*) from vectors where id like 'w%'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n as usize, kept.len());
        let all = cache
            .query(&pool, "t", "c", &kept[0].1, 100, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 100.min(kept.len() + 200));
        let sql = scan_sqlite(&pool, "c", &kept[0].1, 100, None)
            .await
            .unwrap();
        assert_eq!(
            all.iter().map(|h| &h.0).collect::<Vec<_>>(),
            sql.iter().map(|h| &h.0).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn load_never_exceeds_global_cap_when_eviction_blocked() {
        let (_d, pool) = setup().await;
        // one collection: 100 x 256 x 4 = ~100 KB + overhead; cap fits exactly one
        let cache = VecCache::new(cfg(150_000));
        let mut rng = Rng(77);
        for c in ["a", "b"] {
            cache.upsert(&pool, "t", c, 256, None, rows(&mut rng, 100, 256, c)).await.unwrap();
        }
        let q = rng.unit(256);
        cache.query(&pool, "t", "a", &q, 1, None).await.unwrap();
        assert!(cache.is_loaded("t", "a").await);
        // a reader is inside "a": it cannot be evicted
        let guard = cache.slot("t", "a").state.clone().read_owned().await;
        let hit = cache.query(&pool, "t", "b", &q, 3, None).await.unwrap();
        let want = scan_sqlite(&pool, "b", &q, 3, None).await.unwrap();
        assert_eq!(hit.iter().map(|h| &h.0).collect::<Vec<_>>(), want.iter().map(|h| &h.0).collect::<Vec<_>>());
        assert!(cache.total_bytes() <= 150_000, "total {}", cache.total_bytes());
        assert!(!cache.is_loaded("t", "b").await, "b served by scan, not cached");
        drop(guard);
        // once the reader is gone, b can displace a
        cache.query(&pool, "t", "b", &q, 1, None).await.unwrap();
        assert!(cache.is_loaded("t", "b").await);
        assert!(cache.total_bytes() <= 150_000);
        assert_accounting(&cache);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_loads_respect_cap() {
        let (_d, pool) = setup().await;
        let cap = 260_000; // fits two ~100 KB collections, not eight
        let cache = VecCache::new(cfg(cap));
        let mut rng = Rng(5);
        let names: Vec<String> = (0..8).map(|i| format!("c{i}")).collect();
        for c in &names {
            cache.upsert(&pool, "t", c, 256, None, rows(&mut rng, 100, 256, c)).await.unwrap();
        }
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for c in names {
            let (cache, pool, peak) = (cache.clone(), pool.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                let mut rng = Rng(c.len() as u64 + c.as_bytes()[1] as u64);
                for _ in 0..5 {
                    let q = rng.unit(256);
                    let hits = cache.query(&pool, "t", &c, &q, 3, None).await.unwrap();
                    assert_eq!(hits.len(), 3);
                    peak.fetch_max(cache.held_bytes(), AtomicOrdering::SeqCst);
                    tokio::task::yield_now().await;
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        peak.fetch_max(cache.held_bytes(), AtomicOrdering::SeqCst);
        assert!(peak.load(AtomicOrdering::SeqCst) <= cap, "peak {}", peak.load(AtomicOrdering::SeqCst));
        assert!(cache.total_bytes() <= cap);
    }

    #[tokio::test]
    async fn upsert_growth_counts_against_global_total() {
        let (_d, pool) = setup().await;
        // each collection ~100 KB; cap fits both but not both plus growth
        let cache = VecCache::new(cfg(260_000));
        let mut rng = Rng(31);
        for c in ["a", "b"] {
            cache.upsert(&pool, "t", c, 256, None, rows(&mut rng, 100, 256, c)).await.unwrap();
        }
        let q = rng.unit(256);
        cache.query(&pool, "t", "a", &q, 1, None).await.unwrap();
        cache.query(&pool, "t", "b", &q, 1, None).await.unwrap();
        assert!(cache.is_loaded("t", "a").await && cache.is_loaded("t", "b").await);
        // grow "b": alone far below the cap, but the global total would exceed it
        cache.upsert(&pool, "t", "b", 256, None, rows(&mut rng, 40, 256, "more")).await.unwrap();
        assert!(cache.total_bytes() <= 260_000, "total {}", cache.total_bytes());
        // results stay correct whichever of them got unloaded
        let hit = cache.query(&pool, "t", "b", &q, 5, None).await.unwrap();
        let want = scan_sqlite(&pool, "b", &q, 5, None).await.unwrap();
        assert_eq!(hit.iter().map(|h| &h.0).collect::<Vec<_>>(), want.iter().map(|h| &h.0).collect::<Vec<_>>());
        assert!(cache.total_bytes() <= 260_000);
        // pinned neighbour cannot be evicted: the growing collection unloads itself
        let cache = VecCache::new(cfg(260_000));
        for c in ["x", "y"] {
            cache.upsert(&pool, "t", c, 256, None, rows(&mut rng, 100, 256, c)).await.unwrap();
        }
        cache.query(&pool, "t", "x", &q, 1, None).await.unwrap();
        cache.query(&pool, "t", "y", &q, 1, None).await.unwrap();
        assert!(cache.is_loaded("t", "x").await && cache.is_loaded("t", "y").await);
        let guard = cache.slot("t", "x").state.clone().read_owned().await;
        cache.upsert(&pool, "t", "y", 256, None, rows(&mut rng, 40, 256, "evenmore")).await.unwrap();
        drop(guard);
        assert!(cache.total_bytes() <= 260_000, "total {}", cache.total_bytes());
        assert!(!cache.is_loaded("t", "y").await, "y unloaded instead of overcommitting");
        assert!(cache.is_loaded("t", "x").await);
        let hit = cache.query(&pool, "t", "y", &q, 5, None).await.unwrap();
        let want = scan_sqlite(&pool, "y", &q, 5, None).await.unwrap();
        assert_eq!(hit.iter().map(|h| &h.0).collect::<Vec<_>>(), want.iter().map(|h| &h.0).collect::<Vec<_>>());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn eviction_during_load_keeps_accounting_exact() {
        let (_d, pool) = setup().await;
        let cap = 150_000; // one ~120 KB collection at a time: constant eviction pressure
        let cache = VecCache::new(cfg(cap));
        let mut rng = Rng(8);
        let names = ["a", "b", "c", "d", "e", "f"];
        for c in names {
            cache.upsert(&pool, "t", c, 256, None, rows(&mut rng, 100, 256, c)).await.unwrap();
        }
        let mut tasks = Vec::new();
        for t in 0..16usize {
            let (cache, pool) = (cache.clone(), pool.clone());
            tasks.push(tokio::spawn(async move {
                let mut rng = Rng(100 + t as u64);
                for i in 0..40 {
                    let c = names[(t * 5 + i) % names.len()];
                    let q = rng.unit(256);
                    assert_eq!(cache.query(&pool, "t", c, &q, 2, None).await.unwrap().len(), 2);
                    if i % 7 == 0 {
                        let row = rows(&mut rng, 1, 256, &format!("x{t}-{i}-"));
                        cache.upsert(&pool, "t", c, 256, None, row).await.unwrap();
                    }
                    tokio::task::yield_now().await;
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_accounting(&cache);
        assert!(cache.total_bytes() <= cap);
        // idle eviction and invalidation keep it exact too
        cache.evict_idle(i64::MAX);
        assert_accounting(&cache);
        assert_eq!(cache.total_bytes(), 0);
    }

    /// Run with: cargo test --release -- --ignored --nocapture query_20k_1024d_fast
    #[tokio::test]
    #[ignore]
    async fn query_20k_1024d_fast() {
        let (_d, pool) = setup().await;
        let cache = VecCache::new(CacheConfig::default());
        let mut rng = Rng(0xDEADBEEF);
        let dim = 1024;
        for chunk in 0..20 {
            cache
                .upsert(
                    &pool,
                    "t",
                    "big",
                    dim,
                    None,
                    rows(&mut rng, 1000, dim, &format!("c{chunk}-")),
                )
                .await
                .unwrap();
        }
        let t = std::time::Instant::now();
        cache
            .query(&pool, "t", "big", &rng.unit(dim), 5, None)
            .await
            .unwrap();
        println!("cold load + first query (20000 x {dim}): {:?}", t.elapsed());
        let mut times = Vec::new();
        for _ in 0..30 {
            let q = rng.unit(dim);
            let t = std::time::Instant::now();
            cache.query(&pool, "t", "big", &q, 5, None).await.unwrap();
            times.push(t.elapsed());
        }
        times.sort();
        let median = times[times.len() / 2];
        println!(
            "warm query median over 30: {median:?} (min {:?}, max {:?})",
            times[0], times[29]
        );
        assert!(
            median < std::time::Duration::from_millis(20),
            "median {median:?}"
        );
    }
}
