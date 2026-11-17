use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lib_rpc::Rpc;

pub const NEXT_ID_KEY: &str = "meta:next_id";
pub const KEY_PREFIX: &str = "clip:";
pub const DEFAULT_HISTORY_LIMIT: usize = 1000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipEntry {
    pub id: String,
    pub text: String,
    pub created_at_ms: i64,
    pub provider: String,
}

pub struct Db {
    rpc: Rpc,
    timeout_ms: u32,
}

impl Db {
    pub fn new(rpc: Rpc, timeout_ms: u32) -> Self {
        Self { rpc, timeout_ms }
    }

    async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
        self.rpc.call(action, params, self.timeout_ms).await
    }

    pub async fn next_id(&self) -> Result<u64, String> {
        let v = self
            .call("db_incr", serde_json::json!({"key": NEXT_ID_KEY}))
            .await?;
        v.get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("db_incr bad payload: {v}"))
    }

    pub async fn put(&self, entry: &ClipEntry) -> Result<(), String> {
        let key = format!("{KEY_PREFIX}{}", entry.id);
        let v = self
            .call("db_set", serde_json::json!({"key": key, "value": entry}))
            .await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("db_set bad payload: {v}"));
        }
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<ClipEntry>, String> {
        let v = self
            .call(
                "db_get",
                serde_json::json!({"key": format!("{KEY_PREFIX}{id}")}),
            )
            .await?;
        if v.get("found").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let value = v.get("value").cloned().unwrap_or(Value::Null);
        let e: ClipEntry =
            serde_json::from_value(value).map_err(|e| format!("corrupt clip {id:?}: {e}"))?;
        Ok(Some(e))
    }

    pub async fn list(&self) -> Result<Vec<ClipEntry>, String> {
        let v = self
            .call("db_keys", serde_json::json!({"prefix": KEY_PREFIX}))
            .await?;
        let keys: Vec<String> = v
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("db_keys bad payload: {v}"))?
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.to_string())
            .collect();
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let v = self
            .call("db_batch_get", serde_json::json!({"keys": keys}))
            .await?;
        let values = v
            .get("values")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("db_batch_get bad payload: {v}"))?;
        let mut out = Vec::new();
        for (k, val) in values {
            if val.is_null() {
                continue;
            }
            match serde_json::from_value::<ClipEntry>(val.clone()) {
                Ok(e) => out.push(e),
                Err(e) => eprintln!("[clipboard] skip corrupt {k}: {e}"),
            }
        }
        Ok(out)
    }

    pub async fn delete(&self, id: &str) -> Result<bool, String> {
        let v = self
            .call(
                "db_delete",
                serde_json::json!({"key": format!("{KEY_PREFIX}{id}")}),
            )
            .await?;
        v.get("deleted")
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("db_delete bad payload: {v}"))
    }

    pub async fn append(
        &self,
        text: String,
        provider: String,
        limit: usize,
    ) -> Result<ClipEntry, String> {
        let id = self.next_id().await?.to_string();
        let now = now_ms();
        let entry = ClipEntry {
            id: id.clone(),
            text,
            created_at_ms: now,
            provider,
        };
        self.put(&entry).await?;
        if limit > 0 {
            let mut all = self.list().await?;
            if all.len() > limit {
                all.sort_by_key(|e| e.created_at_ms);
                let to_remove = all.len() - limit;
                for e in all.iter().take(to_remove) {
                    let _ = self.delete(&e.id).await;
                }
            }
        }
        Ok(entry)
    }

    pub async fn clear(&self) -> Result<usize, String> {
        let all = self.list().await?;
        let n = all.len();
        for e in all {
            let _ = self.delete(&e.id).await;
        }
        Ok(n)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
