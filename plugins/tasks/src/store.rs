//! Typed access to the `database` plugin on behalf of `tasks`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Rpc;

pub const NEXT_ID_KEY: &str = "meta:next_id";
pub const KEY_PREFIX: &str = "task:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default = "default_list")]
    pub list: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub due_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(default)]
    pub done_at_ms: Option<i64>,
}

fn default_list() -> String {
    "default".into()
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
    pub async fn put(&self, task: &Task) -> Result<(), String> {
        let key = format!("{KEY_PREFIX}{}", task.id);
        let v = self
            .call("db_set", serde_json::json!({"key": key, "value": task}))
            .await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("db_set bad payload: {v}"));
        }
        Ok(())
    }
    pub async fn get(&self, id: &str) -> Result<Option<Task>, String> {
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
        let task: Task =
            serde_json::from_value(value).map_err(|e| format!("corrupt task {id:?}: {e}"))?;
        Ok(Some(task))
    }
    pub async fn list(&self) -> Result<Vec<Task>, String> {
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
            match serde_json::from_value::<Task>(val.clone()) {
                Ok(t) => out.push(t),
                Err(e) => eprintln!("[tasks] skip corrupt {k}: {e}"),
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
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
