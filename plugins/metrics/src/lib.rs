pub mod request;
pub mod sampler;
pub mod store;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use store::Db;

#[derive(Debug, Clone)]
pub struct Config {
    pub interval_secs: u64,
    pub max_samples: usize,
    pub db_timeout_ms: u32,
}

impl Default for Config {
    fn default() -> Self { Self { interval_secs: 30, max_samples: 10000, db_timeout_ms: 5000 } }
}

impl Config {
    pub fn from_env() -> Self {
        let interval_secs = std::env::var("METRICS_PLUGIN_INTERVAL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let max_samples = std::env::var("METRICS_PLUGIN_MAX_SAMPLES").ok().and_then(|s| s.parse().ok()).unwrap_or(10000);
        let db_timeout_ms = std::env::var("METRICS_PLUGIN_DB_TIMEOUT_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(5000);
        Self { interval_secs, max_samples, db_timeout_ms }
    }
}

pub struct RpcCall { pub action: String, pub params_json: Vec<u8>, pub timeout_ms: u32, pub reply: oneshot::Sender<Result<Value, String>> }
#[derive(Clone)]
pub struct Rpc { tx: mpsc::Sender<RpcCall> }
impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self { Self { tx } }
    pub async fn call(&self, action: &str, params: Value, timeout_ms: u32) -> Result<Value, String> {
        let params_json = serde_json::to_vec(&params).map_err(|e| format!("failed to encode {action} params: {e}"))?;
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCall { action: action.to_string(), params_json, timeout_ms, reply }).await.map_err(|_| format!("database.{action} aborted"))?;
        let effective = if timeout_ms==0 {30_000} else {timeout_ms};
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!("database.{action} aborted")),
            Err(_) => Err(format!("database.{action} timed out after {effective} ms")),
        }
    }
}

#[derive(Debug)]
pub struct ActionResult { pub data: Vec<u8>, pub event: Option<(String, Value)> }

pub async fn handle_action(rpc: Rpc, config: &Config, action: &str, params_json: &[u8], start: std::time::Instant) -> Result<ActionResult, String> {
    let req = request::parse_request(action, params_json)?;
    let db = Db::new(rpc.clone(), config.db_timeout_ms);
    match req {
        request::MetricsRequest::Query { from_ms, to_ms, limit, offset } => {
            let mut samples = db.list().await?;
            if let Some(from) = from_ms { samples.retain(|s| s.timestamp_ms >= from); }
            if let Some(to) = to_ms { samples.retain(|s| s.timestamp_ms <= to); }
            samples.sort_by(|a,b| b.timestamp_ms.cmp(&a.timestamp_ms).then_with(|| a.id.cmp(&b.id)));
            let total = samples.len();
            let page: Vec<&sampler::Sample> = samples.iter().skip(offset).take(limit).collect();
            ok(json!({"samples": page, "total": total}), None)
        }
        request::MetricsRequest::Latest => {
            let mut samples = db.list().await?;
            samples.sort_by(|a,b| b.timestamp_ms.cmp(&a.timestamp_ms));
            if let Some(s) = samples.into_iter().next() {
                ok(json!({"found": true, "sample": s}), None)
            } else {
                ok(json!({"found": false, "sample": Value::Null}), None)
            }
        }
        request::MetricsRequest::Stats => {
            let samples = db.list().await?;
            let count = samples.len();
            let oldest_ms = samples.iter().map(|s| s.timestamp_ms).min();
            let newest_ms = samples.iter().map(|s| s.timestamp_ms).max();
            ok(json!({"count": count, "oldest_ms": oldest_ms, "newest_ms": newest_ms}), None)
        }
        request::MetricsRequest::Status => {
            let uptime_ms = start.elapsed().as_millis() as u64;
            ok(json!({"version": env!("CARGO_PKG_VERSION"), "uptime_ms": uptime_ms, "engine_ready": true, "last_error": Value::Null, "counters": {}}), None)
        }
    }
}

pub async fn sample_and_store(rpc: Rpc, config: &Config) -> Result<String, String> {
    let db = Db::new(rpc.clone(), config.db_timeout_ms);
    let id = db.next_id().await?.to_string();
    let now = store::now_ms();
    let sample = sampler::sample_now(id.clone(), now);
    db.put(&sample).await?;
    let _ = db.trim(config.max_samples).await;
    // best-effort event publish handled by caller via outbound channel
    Ok(id)
}

fn ok(data: Value, event: Option<(String, Value)>) -> Result<ActionResult, String> {
    let data = serde_json::to_vec(&data).map_err(|e| format!("encode: {e}"))?;
    Ok(ActionResult { data, event: event.map(|(t,p)| (t,p)) })
}
