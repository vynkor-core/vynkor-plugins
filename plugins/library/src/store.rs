use crate::Rpc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const KEY_PREFIX: &str = "library:";
pub const META_LAST_SCAN: &str = "library:meta:last_scan_ms";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub ext: String,
    pub size_bytes: u64,
    pub mtime_ms: i64,
    pub indexed_at_ms: i64,
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

    pub async fn put_entry(&self, entry: &Entry) -> Result<(), String> {
        let key = format!("{}{}", KEY_PREFIX, entry.id);
        let v = self
            .call("db_set", serde_json::json!({"key": key, "value": entry}))
            .await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("db_set bad payload: {v}"));
        }
        Ok(())
    }

    pub async fn get_entry(&self, id: &str) -> Result<Option<Entry>, String> {
        let v = self
            .call(
                "db_get",
                serde_json::json!({"key": format!("{}{}", KEY_PREFIX, id)}),
            )
            .await?;
        if v.get("found").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let val = v.get("value").cloned().unwrap_or(Value::Null);
        let e: Entry =
            serde_json::from_value(val).map_err(|e| format!("corrupt entry {id}: {e}"))?;
        Ok(Some(e))
    }

    pub async fn list_entries(&self) -> Result<Vec<Entry>, String> {
        let v = self
            .call("db_keys", serde_json::json!({"prefix": KEY_PREFIX}))
            .await?;
        let keys: Vec<String> = v
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("db_keys bad payload: {v}"))?
            .iter()
            .filter_map(Value::as_str)
            .filter(|k| !k.ends_with("meta:last_scan_ms"))
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
            if k.ends_with("meta:last_scan_ms") {
                continue;
            }
            match serde_json::from_value::<Entry>(val.clone()) {
                Ok(e) => out.push(e),
                Err(e) => eprintln!("[library] skip corrupt {k}: {e}"),
            }
        }
        Ok(out)
    }

    pub async fn delete_entry(&self, id: &str) -> Result<bool, String> {
        let v = self
            .call(
                "db_delete",
                serde_json::json!({"key": format!("{}{}", KEY_PREFIX, id)}),
            )
            .await?;
        v.get("deleted")
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("db_delete bad payload: {v}"))
    }

    pub async fn set_last_scan(&self, ms: i64) -> Result<(), String> {
        let v = self
            .call(
                "db_set",
                serde_json::json!({"key": META_LAST_SCAN, "value": ms}),
            )
            .await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("db_set bad payload: {v}"));
        }
        Ok(())
    }

    pub async fn get_last_scan(&self) -> Result<Option<i64>, String> {
        let v = self
            .call("db_get", serde_json::json!({"key": META_LAST_SCAN}))
            .await?;
        if v.get("found").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let val = v.get("value").cloned().unwrap_or(Value::Null);
        if let Some(n) = val.as_i64() {
            Ok(Some(n))
        } else {
            Ok(None)
        }
    }

    pub async fn find_by_path(&self, path: &str) -> Result<Option<Entry>, String> {
        let all = self.list_entries().await?;
        Ok(all.into_iter().find(|e| e.path == path))
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub fn id_for_path(path: &str) -> String {
    // deterministic id: hex of path hash using simple fnv
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn kind_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "wma" | "opus" | "aiff" => "audio",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "bmp" | "svg" | "tiff" => "photo",
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg" => "video",
        _ => "other",
    }
}
