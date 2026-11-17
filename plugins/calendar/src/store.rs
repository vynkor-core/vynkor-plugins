//! Typed access to the `database` plugin on behalf of `calendar`.
//!
//! Same contract as `notes`: kernel-routed actions only via the [`Rpc`]
//! proxy, private namespace stamped by the kernel's `caller_plugin_id`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Rpc;

/// Counter key backing event ids (atomic via `db_incr`).
pub const NEXT_ID_KEY: &str = "meta:next_id";
/// Key prefix for event documents: `event:<id>` → JSON [`EventDoc`].
pub const KEY_PREFIX: &str = "event:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventDoc {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub start_ms: i64,
    #[serde(default)]
    pub end_ms: Option<i64>,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default)]
    pub remind_before_ms: Option<i64>,
    #[serde(default)]
    pub reminder_fired: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Stable external id from an ICS import (`UID` property); re-imports of
    /// the same calendar update in place instead of duplicating events.
    #[serde(default)]
    pub ics_uid: Option<String>,
    #[serde(default)]
    pub rrule: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Typed wrapper over the `database` actions used by calendar.
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

    /// Next monotonic event id (atomic counter in our own namespace).
    pub async fn next_id(&self) -> Result<u64, String> {
        let v = self
            .call("db_incr", serde_json::json!({"key": NEXT_ID_KEY}))
            .await?;
        v.get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("database.db_incr returned unexpected payload: {v}"))
    }

    pub async fn put(&self, event: &EventDoc) -> Result<(), String> {
        let key = format!("{KEY_PREFIX}{}", event.id);
        let v = self
            .call("db_set", serde_json::json!({"key": key, "value": event}))
            .await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("database.db_set returned unexpected payload: {v}"));
        }
        Ok(())
    }

    /// Missing events read as `None`; a present-but-corrupt document is an
    /// error (loudness over silent data loss on single-doc reads).
    pub async fn get(&self, id: &str) -> Result<Option<EventDoc>, String> {
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
        let event: EventDoc = serde_json::from_value(value)
            .map_err(|e| format!("stored event {id:?} is corrupt: {e}"))?;
        Ok(Some(event))
    }

    /// All stored events. Corrupt documents are skipped with a stderr
    /// warning rather than failing the whole listing.
    pub async fn list(&self) -> Result<Vec<EventDoc>, String> {
        let v = self
            .call("db_keys", serde_json::json!({"prefix": KEY_PREFIX}))
            .await?;
        let keys: Vec<String> = v
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("database.db_keys returned unexpected payload: {v}"))?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
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
            .ok_or_else(|| format!("database.db_batch_get returned unexpected payload: {v}"))?;
        let mut events = Vec::new();
        for (key, value) in values {
            if value.is_null() {
                continue;
            }
            match serde_json::from_value::<EventDoc>(value.clone()) {
                Ok(event) => events.push(event),
                Err(e) => eprintln!("[calendar] skipping corrupt document {key}: {e}"),
            }
        }
        Ok(events)
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
            .ok_or_else(|| format!("database.db_delete returned unexpected payload: {v}"))
    }
}

/// Current unix time in milliseconds (same saturating pattern as `database`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
