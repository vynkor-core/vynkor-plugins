use crate::Rpc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const NEXT_FEED_ID: &str = "meta:next_feed_id";
pub const NEXT_ARTICLE_ID: &str = "meta:next_article_id";
pub const FEED_PREFIX: &str = "feed:";
pub const ARTICLE_PREFIX: &str = "article:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feed {
    pub id: String,
    pub url: String,
    pub title: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Article {
    pub id: String,
    pub feed_id: String,
    pub title: String,
    pub link: String,
    pub published_ms: Option<i64>,
    pub read: bool,
    pub created_at_ms: i64,
}

pub struct Db {
    rpc: Rpc,
    timeout_ms: u32,
}
impl Db {
    pub fn new(rpc: Rpc, timeout_ms: u32) -> Self {
        Self { rpc, timeout_ms }
    }
    pub async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
        self.rpc.call(action, params, self.timeout_ms).await
    }
    pub async fn next_id(&self, key: &str) -> Result<u64, String> {
        let v = self
            .call("db_incr", serde_json::json!({"key": key}))
            .await?;
        v.get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("db_incr bad: {v}"))
    }
    pub async fn put_feed(&self, f: &Feed) -> Result<(), String> {
        let key = format!("{FEED_PREFIX}{}", f.id);
        let v = self
            .call("db_set", serde_json::json!({"key": key, "value": f}))
            .await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("db_set bad: {v}"));
        }
        Ok(())
    }
    pub async fn get_feed(&self, id: &str) -> Result<Option<Feed>, String> {
        let v = self
            .call(
                "db_get",
                serde_json::json!({"key": format!("{FEED_PREFIX}{id}")}),
            )
            .await?;
        if v.get("found").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let val = v.get("value").cloned().unwrap_or(Value::Null);
        let f: Feed = serde_json::from_value(val).map_err(|e| format!("corrupt feed {id}: {e}"))?;
        Ok(Some(f))
    }
    pub async fn list_feeds(&self) -> Result<Vec<Feed>, String> {
        let v = self
            .call("db_keys", serde_json::json!({"prefix": FEED_PREFIX}))
            .await?;
        let keys: Vec<String> = v
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("db_keys bad: {v}"))?
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
            .ok_or_else(|| format!("db_batch_get bad: {v}"))?;
        let mut out = Vec::new();
        for (k, val) in values {
            if val.is_null() {
                continue;
            }
            match serde_json::from_value::<Feed>(val.clone()) {
                Ok(f) => out.push(f),
                Err(e) => eprintln!("[rss] skip corrupt {k}: {e}"),
            }
        }
        Ok(out)
    }
    pub async fn delete_feed(&self, id: &str) -> Result<bool, String> {
        let v = self
            .call(
                "db_delete",
                serde_json::json!({"key": format!("{FEED_PREFIX}{id}")}),
            )
            .await?;
        v.get("deleted")
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("db_delete bad: {v}"))
    }
    pub async fn put_article(&self, a: &Article) -> Result<(), String> {
        let key = format!("{ARTICLE_PREFIX}{}", a.id);
        let v = self
            .call("db_set", serde_json::json!({"key": key, "value": a}))
            .await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("db_set bad: {v}"));
        }
        Ok(())
    }
    pub async fn get_article(&self, id: &str) -> Result<Option<Article>, String> {
        let v = self
            .call(
                "db_get",
                serde_json::json!({"key": format!("{ARTICLE_PREFIX}{id}")}),
            )
            .await?;
        if v.get("found").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let val = v.get("value").cloned().unwrap_or(Value::Null);
        let a: Article =
            serde_json::from_value(val).map_err(|e| format!("corrupt article {id}: {e}"))?;
        Ok(Some(a))
    }
    pub async fn list_articles(&self) -> Result<Vec<Article>, String> {
        let v = self
            .call("db_keys", serde_json::json!({"prefix": ARTICLE_PREFIX}))
            .await?;
        let keys: Vec<String> = v
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("db_keys bad: {v}"))?
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
            .ok_or_else(|| format!("db_batch_get bad: {v}"))?;
        let mut out = Vec::new();
        for (k, val) in values {
            if val.is_null() {
                continue;
            }
            match serde_json::from_value::<Article>(val.clone()) {
                Ok(a) => out.push(a),
                Err(e) => eprintln!("[rss] skip corrupt {k}: {e}"),
            }
        }
        Ok(out)
    }
    pub async fn find_article_by_link(&self, link: &str) -> Result<Option<Article>, String> {
        let all = self.list_articles().await?;
        Ok(all.into_iter().find(|a| a.link == link))
    }
}
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
