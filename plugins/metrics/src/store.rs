use serde_json::Value;
use crate::Rpc;
use crate::sampler::Sample;

pub const NEXT_ID_KEY: &str = "meta:next_id";
pub const KEY_PREFIX: &str = "metric:";
const BATCH_KEYS: usize = 500;
const LATEST_PROBE: usize = 8;
const MAX_TRIM_PER_CALL: usize = 200;

pub struct Db {
    rpc: Rpc,
    timeout_ms: u32,
}

impl Db {
    pub fn new(rpc: Rpc, timeout_ms: u32) -> Self { Self { rpc, timeout_ms } }
    async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
        self.rpc.call(action, params, self.timeout_ms).await
    }
    pub async fn next_id(&self) -> Result<u64, String> {
        let v = self.call("db_incr", serde_json::json!({"key": NEXT_ID_KEY})).await?;
        v.get("value").and_then(Value::as_u64).ok_or_else(|| format!("db_incr bad: {v}"))
    }
    pub async fn put(&self, sample: &Sample) -> Result<(), String> {
        let key = format!("{KEY_PREFIX}{}", sample.id);
        let v = self.call("db_set", serde_json::json!({"key": key, "value": sample})).await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) { return Err(format!("db_set bad: {v}")); }
        Ok(())
    }
    /// Sample ids, oldest first. Ids come from `db_incr`, so id order is
    /// write order and nothing needs the values to find the oldest.
    pub async fn ids(&self) -> Result<Vec<u64>, String> {
        let v = self.call("db_keys", serde_json::json!({"prefix": KEY_PREFIX})).await?;
        let keys = v.get("keys").and_then(Value::as_array).ok_or_else(|| format!("db_keys bad: {v}"))?;
        let mut ids: Vec<u64> = keys.iter().filter_map(Value::as_str).filter_map(|k| k.strip_prefix(KEY_PREFIX)?.parse().ok()).collect();
        ids.sort_unstable();
        Ok(ids)
    }
    /// Samples for `ids`, fetched `BATCH_KEYS` at a time: one batch of every
    /// key passed the database plugin's 4 MiB response cap at ~14k samples
    /// (~300 bytes each), which failed every action and the trim with it.
    pub async fn get_many(&self, ids: &[u64]) -> Result<Vec<Sample>, String> {
        let mut out = Vec::new();
        for chunk in ids.chunks(BATCH_KEYS) {
            let keys: Vec<String> = chunk.iter().map(|id| format!("{KEY_PREFIX}{id}")).collect();
            let v = self.call("db_batch_get", serde_json::json!({"keys": keys})).await?;
            let values = v.get("values").and_then(Value::as_object).ok_or_else(|| format!("db_batch_get bad: {v}"))?;
            for (k, val) in values {
                if val.is_null() { continue; }
                match serde_json::from_value::<Sample>(val.clone()) {
                    Ok(s) => out.push(s),
                    Err(e) => eprintln!("[metrics] skip corrupt {k}: {e}"),
                }
            }
        }
        Ok(out)
    }
    pub async fn list(&self) -> Result<Vec<Sample>, String> {
        let ids = self.ids().await?;
        self.get_many(&ids).await
    }
    /// The newest readable sample, without loading the rest.
    pub async fn latest(&self) -> Result<Option<Sample>, String> {
        let ids = self.ids().await?;
        self.first_readable(ids.rchunks(LATEST_PROBE), true).await
    }
    /// Oldest and newest readable samples. Probes a few ids at each end:
    /// the trim may delete the oldest between listing ids and reading it.
    pub async fn ends(&self) -> Result<(usize, Option<Sample>, Option<Sample>), String> {
        let ids = self.ids().await?;
        let oldest = self.first_readable(ids.chunks(LATEST_PROBE), false).await?;
        let newest = self.first_readable(ids.rchunks(LATEST_PROBE), true).await?;
        Ok((ids.len(), oldest, newest))
    }
    async fn first_readable<'a>(&self, chunks: impl Iterator<Item = &'a [u64]>, newest: bool) -> Result<Option<Sample>, String> {
        for chunk in chunks {
            let got = self.get_many(chunk).await?.into_iter();
            let pick = if newest { got.max_by_key(|s| s.timestamp_ms) } else { got.min_by_key(|s| s.timestamp_ms) };
            if pick.is_some() {
                return Ok(pick);
            }
        }
        Ok(None)
    }
    pub async fn delete(&self, id: &str) -> Result<bool, String> {
        let v = self.call("db_delete", serde_json::json!({"key": format!("{KEY_PREFIX}{id}")})).await?;
        v.get("deleted").and_then(Value::as_bool).ok_or_else(|| format!("db_delete bad: {v}"))
    }
    /// Deletes the oldest samples past `max_samples`, at most
    /// `MAX_TRIM_PER_CALL` per call. Every `db_delete` publishes a change
    /// event: 2000 a tick (draining the 46k backlog left by the old failing
    /// trim) timed deletes out and twice dropped the database plugin's
    /// kernel connection, so a backlog drains slowly instead.
    pub async fn trim(&self, max_samples: usize) -> Result<usize, String> {
        if max_samples == 0 { return Ok(0); }
        let ids = self.ids().await?;
        if ids.len() <= max_samples { return Ok(0); }
        let to_remove = (ids.len() - max_samples).min(MAX_TRIM_PER_CALL);
        let mut removed = 0;
        for id in &ids[..to_remove] {
            if self.delete(&id.to_string()).await? { removed += 1; }
        }
        Ok(removed)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)).unwrap_or(0)
}
