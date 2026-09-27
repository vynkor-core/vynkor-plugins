pub mod request;
pub mod store;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use store::{Check, Target, TargetState};

#[derive(Debug, Clone)]
pub struct Config {
    pub interval_secs: u64,
    pub max_checks: usize,
    pub db_timeout_ms: u32,
    pub check_timeout_ms: u32,
    /// Consecutive failed background checks before `check_failed` fires.
    pub fail_threshold: u32,
}
impl Default for Config {
    fn default() -> Self { Self { interval_secs: 60, max_checks: 5000, db_timeout_ms: 5000, check_timeout_ms: 5000, fail_threshold: 2 } }
}
impl Config {
    pub fn from_env() -> Self {
        let interval_secs = std::env::var("UPTIME_PLUGIN_INTERVAL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        let max_checks = std::env::var("UPTIME_PLUGIN_MAX_CHECKS").ok().and_then(|s| s.parse().ok()).unwrap_or(5000);
        let db_timeout_ms = std::env::var("UPTIME_PLUGIN_DB_TIMEOUT_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(5000);
        let check_timeout_ms = std::env::var("UPTIME_PLUGIN_CHECK_TIMEOUT_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(5000);
        let fail_threshold = std::env::var("UPTIME_PLUGIN_FAIL_THRESHOLD").ok().and_then(|s| s.parse().ok()).filter(|n: &u32| *n > 0).unwrap_or(2);
        Self { interval_secs, max_checks, db_timeout_ms, check_timeout_ms, fail_threshold }
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
        self.tx.send(RpcCall { action: action.to_string(), params_json, timeout_ms, reply }).await.map_err(|_| format!("{action} aborted"))?;
        let effective = if timeout_ms==0 {30_000} else {timeout_ms};
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!("{action} aborted")),
            Err(_) => Err(format!("{action} timed out after {effective} ms")),
        }
    }
}

#[derive(Debug)]
pub struct ActionResult { pub data: Vec<u8>, pub event: Option<(String, Value)> }

pub async fn handle_action(rpc: Rpc, config: &Config, action: &str, params_json: &[u8], start: std::time::Instant) -> Result<ActionResult, String> {
    let req = request::parse_request(action, params_json)?;
    let db = store::Db::new(rpc.clone(), config.db_timeout_ms);
    match req {
        request::UptimeRequest::Add { url } => {
            // dedup by url
            let existing = db.list_targets().await?;
            if let Some(t) = existing.iter().find(|t| t.url==url) {
                return ok(json!({"id": t.id, "target": t}), None);
            }
            let id = db.next_id(store::NEXT_TARGET_ID).await?.to_string();
            let now = store::now_ms();
            let target = Target { id: id.clone(), url: url.clone(), created_at_ms: now };
            db.put_target(&target).await?;
            ok(json!({"id": id, "target": target}), Some(("target_added".into(), json!({"id": id, "url": url}))))
        }
        request::UptimeRequest::Remove { id } => {
            let removed = db.delete_target(&id).await?;
            let _ = db.delete_state(&id).await;
            ok(json!({"removed": removed}), removed.then(|| ("target_removed".into(), json!({"id": id}))))
        }
        request::UptimeRequest::List => {
            let mut targets = db.list_targets().await?;
            targets.sort_by(|a,b| a.created_at_ms.cmp(&b.created_at_ms));
            ok(json!({"targets": targets}), None)
        }
        request::UptimeRequest::Check { url, timeout_ms } => {
            let res = do_check(rpc.clone(), &url, timeout_ms).await;
            // store check
            let id = db.next_id(store::NEXT_CHECK_ID).await?.to_string();
            let now = store::now_ms();
            let check = Check { id: id.clone(), url: url.clone(), timestamp_ms: now, ok: res.ok, status: res.status, latency_ms: res.latency_ms, error: res.error.clone() };
            let _ = db.put_check(&check).await;
            let _ = db.trim_checks(config.max_checks).await;
            let event = if !res.ok { Some(("check_failed".into(), json!({"url": url, "status": res.status, "error": res.error}))) } else { None };
            ok(json!({"url": url, "ok": res.ok, "status": res.status, "latency_ms": res.latency_ms, "error": res.error}), event)
        }
        request::UptimeRequest::History { url, limit, offset } => {
            let mut checks = db.list_checks().await?;
            if let Some(u) = url { checks.retain(|c| c.url==u); }
            checks.sort_by(|a,b| b.timestamp_ms.cmp(&a.timestamp_ms));
            let total = checks.len();
            let page: Vec<&Check> = checks.iter().skip(offset).take(limit).collect();
            ok(json!({"checks": page, "total": total}), None)
        }
        request::UptimeRequest::Status => {
            let uptime_ms = start.elapsed().as_millis() as u64;
            ok(json!({"version": env!("CARGO_PKG_VERSION"), "uptime_ms": uptime_ms, "engine_ready": true, "last_error": Value::Null, "counters": {}}), None)
        }
    }
}

struct CheckResult { ok: bool, status: i32, latency_ms: u64, error: Option<String> }

async fn do_check(rpc: Rpc, url: &str, timeout_ms: u64) -> CheckResult {
    let start = std::time::Instant::now();
    let http_req = serde_json::json!({"url": url, "method": "GET", "timeout_ms": timeout_ms, "follow_redirects": true});
    let res = rpc.call("http_request", http_req, timeout_ms as u32).await;
    let latency_ms = start.elapsed().as_millis() as u64;
    match res {
        Ok(v) => {
            let status = v.get("status").and_then(Value::as_i64).unwrap_or(0) as i32;
            let ok = (200..300).contains(&status);
            let err = if ok { None } else { Some(format!("HTTP {status}")) };
            CheckResult { ok, status, latency_ms, error: err }
        }
        Err(e) => CheckResult { ok: false, status: 0, latency_ms, error: Some(e) },
    }
}

/// Outcome of one background scan: how many targets were checked, plus the
/// alert events (`check_failed` / `recovered`) for the serve loop to put on
/// the bus.
pub struct ScanResult { pub checked: usize, pub events: Vec<(String, Value)> }

/// Advances a target's outage state by one background check and returns the
/// event to publish, if any. `check_failed` fires once per outage, when the
/// streak reaches `threshold` (a single timeout no longer pages anyone);
/// `recovered` fires on the first OK check after that alert. A streak that
/// heals before reaching the threshold is silent both ways.
fn advance(t: &mut TargetState, url: &str, res: &CheckResult, now: i64, threshold: u32) -> Option<(String, Value)> {
    if res.ok {
        let event = t.alerting.then(|| {
            let down_for_ms = t.down_since_ms.map(|s| (now - s).max(0)).unwrap_or(0);
            ("recovered".to_string(), json!({"url": url, "down_for_ms": down_for_ms, "failures": t.fail_streak}))
        });
        t.fail_streak = 0;
        t.down_since_ms = None;
        t.alerting = false;
        return event;
    }
    t.fail_streak = t.fail_streak.saturating_add(1);
    t.down_since_ms.get_or_insert(now);
    if t.alerting || t.fail_streak < threshold {
        return None;
    }
    t.alerting = true;
    Some(("check_failed".to_string(), json!({"url": url, "status": res.status, "error": res.error, "failures": t.fail_streak})))
}

pub async fn scan_all(rpc: Rpc, config: &Config) -> Result<ScanResult, String> {
    let db = store::Db::new(rpc.clone(), config.db_timeout_ms);
    let targets = db.list_targets().await?;
    let mut states = db.list_states().await?;
    let mut scan = ScanResult { checked: 0, events: Vec::new() };
    for t in targets {
        let res = do_check(rpc.clone(), &t.url, config.check_timeout_ms as u64).await;
        let id = db.next_id(store::NEXT_CHECK_ID).await?.to_string();
        let now = store::now_ms();
        let mut st = states.remove(&t.id).unwrap_or_default();
        let before = st.clone();
        let event = advance(&mut st, &t.url, &res, now, config.fail_threshold);
        // Persist before publishing: if the write fails, the event is dropped
        // and the next scan re-derives the same transition instead of
        // alerting twice.
        let persisted = st == before || match db.put_state(&t.id, &st).await {
            Ok(()) => true,
            Err(e) => { eprintln!("[uptime] failed to persist state for {}: {e}", t.url); false }
        };
        if persisted { scan.events.extend(event); }
        let check = Check { id, url: t.url.clone(), timestamp_ms: now, ok: res.ok, status: res.status, latency_ms: res.latency_ms, error: res.error };
        let _ = db.put_check(&check).await;
        scan.checked+=1;
    }
    let _ = db.trim_checks(config.max_checks).await;
    Ok(scan)
}

fn ok(data: Value, event: Option<(String, Value)>) -> Result<ActionResult, String> {
    let data = serde_json::to_vec(&data).map_err(|e| format!("encode: {e}"))?;
    Ok(ActionResult { data, event })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Fake kernel for `scan_all`: in-memory `db_*`, `http_request` answers
    /// 200 for URLs containing "up" and 503 otherwise.
    fn spawn_fake(mut rx: mpsc::Receiver<RpcCall>) {
        tokio::spawn(async move {
            let mut kv: BTreeMap<String, Value> = BTreeMap::new();
            while let Some(call) = rx.recv().await {
                let p: Value = serde_json::from_slice(&call.params_json).unwrap();
                let key = p.get("key").and_then(Value::as_str).unwrap_or_default().to_string();
                let res = match call.action.as_str() {
                    "db_incr" => { let n = kv.get(&key).and_then(Value::as_i64).unwrap_or(0) + 1; kv.insert(key, json!(n)); json!({"ok": true, "value": n}) }
                    "db_set" => { kv.insert(key, p["value"].clone()); json!({"ok": true}) }
                    "db_get" => match kv.get(&key) { Some(v) => json!({"found": true, "value": v}), None => json!({"found": false, "value": null}) },
                    "db_keys" => { let pre = p["prefix"].as_str().unwrap_or(""); json!({"keys": kv.keys().filter(|k| k.starts_with(pre)).collect::<Vec<_>>()}) }
                    "db_batch_get" => { let mut m = serde_json::Map::new(); for k in p["keys"].as_array().unwrap() { let k = k.as_str().unwrap(); m.insert(k.into(), kv.get(k).cloned().unwrap_or(Value::Null)); } json!({"values": m}) }
                    "db_delete" => json!({"deleted": kv.remove(&key).is_some()}),
                    "http_request" => { let up = p["url"].as_str().unwrap().contains("up"); json!({"status": if up {200} else {503}, "body": "", "body_encoding": "utf8"}) }
                    other => { let _ = call.reply.send(Err(format!("unknown {other}"))); continue; }
                };
                let _ = call.reply.send(Ok(res));
            }
        });
    }

    #[tokio::test]
    async fn scan_all_reports_failed_targets_as_events() {
        let (tx, rx) = mpsc::channel(16);
        spawn_fake(rx);
        let rpc = Rpc::new(tx);
        let cfg = Config { fail_threshold: 1, ..Config::default() };
        for url in ["https://up.example", "https://down.example"] {
            let req = serde_json::to_vec(&json!({"url": url})).unwrap();
            handle_action(rpc.clone(), &cfg, "uptime_add", &req, std::time::Instant::now()).await.unwrap();
        }
        let scan = scan_all(rpc, &cfg).await.unwrap();
        assert_eq!(scan.checked, 2);
        assert_eq!(scan.events.len(), 1, "only the 503 target is reported");
        let (event_type, payload) = &scan.events[0];
        assert_eq!(event_type, "check_failed");
        assert_eq!(payload["url"], "https://down.example");
        assert_eq!(payload["status"], 503);
    }

    #[tokio::test]
    async fn scan_all_alerts_once_after_threshold_and_survives_removal() {
        let (tx, rx) = mpsc::channel(16);
        spawn_fake(rx);
        let rpc = Rpc::new(tx);
        let cfg = Config::default(); // fail_threshold: 2
        let req = serde_json::to_vec(&json!({"url": "https://down.example"})).unwrap();
        handle_action(rpc.clone(), &cfg, "uptime_add", &req, std::time::Instant::now()).await.unwrap();

        let first = scan_all(rpc.clone(), &cfg).await.unwrap();
        assert!(first.events.is_empty(), "one failure is below the threshold");
        let second = scan_all(rpc.clone(), &cfg).await.unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].0, "check_failed");
        assert_eq!(second.events[0].1["failures"], 2);
        let third = scan_all(rpc.clone(), &cfg).await.unwrap();
        assert!(third.events.is_empty(), "an ongoing outage alerts once");

        let req = serde_json::to_vec(&json!({"id": "1"})).unwrap();
        handle_action(rpc.clone(), &cfg, "uptime_remove", &req, std::time::Instant::now()).await.unwrap();
        let db = store::Db::new(rpc.clone(), cfg.db_timeout_ms);
        assert!(db.list_states().await.unwrap().is_empty(), "remove drops the outage state");
        let after = scan_all(rpc, &cfg).await.unwrap();
        assert_eq!(after.checked, 0);
    }

    fn check(ok: bool) -> CheckResult {
        CheckResult { ok, status: if ok { 200 } else { 503 }, latency_ms: 1, error: (!ok).then(|| "HTTP 503".into()) }
    }

    #[test]
    fn advance_alerts_at_threshold_then_recovers_with_downtime() {
        let mut st = TargetState::default();
        assert!(advance(&mut st, "u", &check(false), 1_000, 2).is_none());
        let (ty, p) = advance(&mut st, "u", &check(false), 2_000, 2).unwrap();
        assert_eq!(ty, "check_failed");
        assert_eq!(p["failures"], 2);
        assert!(advance(&mut st, "u", &check(false), 3_000, 2).is_none(), "no repeat while down");
        let (ty, p) = advance(&mut st, "u", &check(true), 4_500, 2).unwrap();
        assert_eq!(ty, "recovered");
        assert_eq!(p["down_for_ms"], 3_500, "measured from the first failure");
        assert_eq!(p["failures"], 3);
        assert_eq!(st, TargetState::default());
    }

    #[test]
    fn advance_is_silent_for_a_blip_below_threshold() {
        let mut st = TargetState::default();
        assert!(advance(&mut st, "u", &check(false), 1_000, 3).is_none());
        assert!(advance(&mut st, "u", &check(false), 2_000, 3).is_none());
        assert!(advance(&mut st, "u", &check(true), 3_000, 3).is_none(), "no recovered without an alert");
        assert_eq!(st, TargetState::default());
    }
}
