use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::Rpc;

pub const NEXT_TARGET_ID: &str = "meta:next_target_id";
pub const NEXT_CHECK_ID: &str = "meta:next_check_id";
pub const TARGET_PREFIX: &str = "target:";
pub const CHECK_PREFIX: &str = "check:";
pub const STATE_PREFIX: &str = "state:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub url: String,
    pub created_at_ms: i64,
}

/// Outage bookkeeping for one target, kept under its own `state:<id>` key so
/// a scan writing it back can never resurrect a target `uptime_remove`
/// deleted mid-scan.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetState {
    /// Consecutive failed background checks; 0 while healthy.
    pub fail_streak: u32,
    /// Timestamp of the first failure in the current streak.
    pub down_since_ms: Option<i64>,
    /// `check_failed` was published for the current outage, so the next OK
    /// check publishes `recovered`.
    pub alerting: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub url: String,
    pub timestamp_ms: i64,
    pub ok: bool,
    pub status: i32,
    pub latency_ms: u64,
    pub error: Option<String>,
}

pub struct Db { rpc: Rpc, timeout_ms: u32 }
impl Db {
    pub fn new(rpc: Rpc, timeout_ms: u32) -> Self { Self { rpc, timeout_ms } }
    async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
        self.rpc.call(action, params, self.timeout_ms).await
    }
    pub async fn next_id(&self, key: &str) -> Result<u64, String> {
        let v = self.call("db_incr", serde_json::json!({"key": key})).await?;
        v.get("value").and_then(Value::as_u64).ok_or_else(|| format!("db_incr bad: {v}"))
    }
    pub async fn put_target(&self, t: &Target) -> Result<(), String> {
        let key = format!("{TARGET_PREFIX}{}", t.id);
        let v = self.call("db_set", serde_json::json!({"key": key, "value": t})).await?;
        if v.get("ok").and_then(Value::as_bool)!=Some(true) { return Err(format!("db_set bad: {v}")); }
        Ok(())
    }
    pub async fn get_target(&self, id: &str) -> Result<Option<Target>, String> {
        let v = self.call("db_get", serde_json::json!({"key": format!("{TARGET_PREFIX}{id}")})).await?;
        if v.get("found").and_then(Value::as_bool)!=Some(true) { return Ok(None); }
        let val = v.get("value").cloned().unwrap_or(Value::Null);
        let t: Target = serde_json::from_value(val).map_err(|e| format!("corrupt target {id}: {e}"))?;
        Ok(Some(t))
    }
    pub async fn list_targets(&self) -> Result<Vec<Target>, String> {
        let v = self.call("db_keys", serde_json::json!({"prefix": TARGET_PREFIX})).await?;
        let keys: Vec<String> = v.get("keys").and_then(Value::as_array).ok_or_else(|| format!("db_keys bad: {v}"))?.iter().filter_map(Value::as_str).map(|s| s.to_string()).collect();
        if keys.is_empty() { return Ok(Vec::new()); }
        let v = self.call("db_batch_get", serde_json::json!({"keys": keys})).await?;
        let values = v.get("values").and_then(Value::as_object).ok_or_else(|| format!("db_batch_get bad: {v}"))?;
        let mut out = Vec::new();
        for (k,val) in values { if val.is_null() {continue;} match serde_json::from_value::<Target>(val.clone()) { Ok(t)=>out.push(t), Err(e)=>eprintln!("[uptime] skip corrupt {k}: {e}") } }
        Ok(out)
    }
    pub async fn delete_target(&self, id: &str) -> Result<bool, String> {
        let v = self.call("db_delete", serde_json::json!({"key": format!("{TARGET_PREFIX}{id}")})).await?;
        v.get("deleted").and_then(Value::as_bool).ok_or_else(|| format!("db_delete bad: {v}"))
    }
    pub async fn list_states(&self) -> Result<std::collections::HashMap<String, TargetState>, String> {
        let v = self.call("db_keys", serde_json::json!({"prefix": STATE_PREFIX})).await?;
        let keys: Vec<String> = v.get("keys").and_then(Value::as_array).ok_or_else(|| format!("db_keys bad: {v}"))?.iter().filter_map(Value::as_str).map(|s| s.to_string()).collect();
        let mut out = std::collections::HashMap::new();
        if keys.is_empty() { return Ok(out); }
        let v = self.call("db_batch_get", serde_json::json!({"keys": keys})).await?;
        let values = v.get("values").and_then(Value::as_object).ok_or_else(|| format!("db_batch_get bad: {v}"))?;
        for (k,val) in values {
            if val.is_null() {continue;}
            let Some(id) = k.strip_prefix(STATE_PREFIX) else {continue};
            match serde_json::from_value::<TargetState>(val.clone()) { Ok(st)=>{ out.insert(id.to_string(), st); }, Err(e)=>eprintln!("[uptime] skip corrupt {k}: {e}") }
        }
        Ok(out)
    }
    pub async fn put_state(&self, target_id: &str, st: &TargetState) -> Result<(), String> {
        let v = self.call("db_set", serde_json::json!({"key": format!("{STATE_PREFIX}{target_id}"), "value": st})).await?;
        if v.get("ok").and_then(Value::as_bool)!=Some(true) { return Err(format!("db_set bad: {v}")); }
        Ok(())
    }
    pub async fn delete_state(&self, target_id: &str) -> Result<(), String> {
        self.call("db_delete", serde_json::json!({"key": format!("{STATE_PREFIX}{target_id}")})).await.map(|_| ())
    }
    pub async fn put_check(&self, c: &Check) -> Result<(), String> {
        let key = format!("{CHECK_PREFIX}{}", c.id);
        let v = self.call("db_set", serde_json::json!({"key": key, "value": c})).await?;
        if v.get("ok").and_then(Value::as_bool)!=Some(true) { return Err(format!("db_set bad: {v}")); }
        Ok(())
    }
    pub async fn list_checks(&self) -> Result<Vec<Check>, String> {
        let v = self.call("db_keys", serde_json::json!({"prefix": CHECK_PREFIX})).await?;
        let keys: Vec<String> = v.get("keys").and_then(Value::as_array).ok_or_else(|| format!("db_keys bad: {v}"))?.iter().filter_map(Value::as_str).map(|s| s.to_string()).collect();
        if keys.is_empty() { return Ok(Vec::new()); }
        let v = self.call("db_batch_get", serde_json::json!({"keys": keys})).await?;
        let values = v.get("values").and_then(Value::as_object).ok_or_else(|| format!("db_batch_get bad: {v}"))?;
        let mut out = Vec::new();
        for (k,val) in values { if val.is_null() {continue;} match serde_json::from_value::<Check>(val.clone()) { Ok(c)=>out.push(c), Err(e)=>eprintln!("[uptime] skip corrupt {k}: {e}") } }
        Ok(out)
    }
    pub async fn trim_checks(&self, max: usize) -> Result<usize, String> {
        if max==0 { return Ok(0); }
        let mut all = self.list_checks().await?;
        if all.len() <= max { return Ok(0); }
        all.sort_by_key(|c| c.timestamp_ms);
        let to_remove = all.len() - max;
        let mut n=0;
        for c in all.iter().take(to_remove) { if self.delete_check(&c.id).await? { n+=1; } }
        Ok(n)
    }
    async fn delete_check(&self, id: &str) -> Result<bool, String> {
        let v = self.call("db_delete", serde_json::json!({"key": format!("{CHECK_PREFIX}{id}")})).await?;
        v.get("deleted").and_then(Value::as_bool).ok_or_else(|| format!("db_delete bad: {v}"))
    }
}
pub fn now_ms() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)).unwrap_or(0) }
