pub mod request;
pub mod scan;
pub mod store;

use rand::seq::SliceRandom;
use serde_json::{json, Value};
use store::{Db, Entry};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct Config {
    pub db_timeout_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_timeout_ms: 5000,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let db_timeout_ms = std::env::var("LIBRARY_PLUGIN_DB_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5000);
        Self { db_timeout_ms }
    }
}

pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self { tx }
    }

    pub async fn call(
        &self,
        action: &str,
        params: Value,
        timeout_ms: u32,
    ) -> Result<Value, String> {
        let params_json = serde_json::to_vec(&params)
            .map_err(|e| format!("failed to encode {action} params: {e}"))?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RpcCall {
                action: action.to_string(),
                params_json,
                timeout_ms,
                reply,
            })
            .await
            .map_err(|_| format!("{action} aborted: serve loop is shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("{action} aborted: serve loop is shutting down")),
            Err(_) => Err(format!("{action} timed out after {effective} ms")),
        }
    }
}

#[derive(Debug)]
pub struct ActionResult {
    pub data: Vec<u8>,
    pub event: Option<(String, Value)>,
}

pub async fn handle_action(
    rpc: Rpc,
    config: &Config,
    action: &str,
    params_json: &[u8],
    _start: std::time::Instant,
) -> Result<ActionResult, String> {
    let req = request::parse_request(action, params_json)?;
    let db = Db::new(rpc.clone(), config.db_timeout_ms);
    match req {
        request::LibraryRequest::Scan { roots, force } => {
            let scan = tokio::task::spawn_blocking(move || scan::scan_filesystem(roots, force))
                .await
                .map_err(|e| format!("scan join failed: {e}"))??;

            let existing = db.list_entries().await?;
            let mut existing_by_path = std::collections::HashMap::new();
            for e in &existing {
                existing_by_path.insert(e.path.clone(), e.clone());
            }

            let mut indexed = 0;
            for entry in scan.entries {
                let needs_update = match existing_by_path.get(&entry.path) {
                    Some(old) => {
                        old.mtime_ms != entry.mtime_ms || old.size_bytes != entry.size_bytes
                    }
                    None => true,
                };
                if needs_update {
                    db.put_entry(&entry).await?;
                    indexed += 1;
                }
                existing_by_path.remove(&entry.path);
            }

            // remaining are stale (deleted files)
            let mut removed = 0;
            for (_, stale) in existing_by_path {
                // only remove if file no longer exists (we already know it's stale because not in scan)
                // But scan may have skipped due to max limit, so be conservative: only remove if not in scanned roots?
                // For now, remove all stale that were not seen
                db.delete_entry(&stale.id).await?;
                removed += 1;
            }

            db.set_last_scan(store::now_ms()).await?;
            let total_entries = db.list_entries().await?.len();

            let event = Some((
                "library_indexed".to_string(),
                json!({"scanned": scan.scanned, "indexed": indexed, "removed": removed}),
            ));

            ok(
                json!({
                    "scanned": scan.scanned,
                    "indexed": indexed,
                    "removed": removed,
                    "total": total_entries
                }),
                event,
            )
        }
        request::LibraryRequest::Search {
            query,
            kind,
            limit,
            offset,
        } => {
            let mut entries = db.list_entries().await?;
            if kind != "all" {
                entries.retain(|e| e.kind == kind);
            }
            if let Some(q) = query {
                let ql = q.to_lowercase();
                entries.retain(|e| {
                    e.name.to_lowercase().contains(&ql) || e.path.to_lowercase().contains(&ql)
                });
            }
            entries.sort_by(|a, b| {
                b.mtime_ms
                    .cmp(&a.mtime_ms)
                    .then_with(|| a.name.cmp(&b.name))
            });
            let total = entries.len();
            let page: Vec<&Entry> = entries.iter().skip(offset).take(limit).collect();
            ok(json!({"results": page, "total": total}), None)
        }
        request::LibraryRequest::Get { id } => match db.get_entry(&id).await? {
            Some(entry) => ok(json!({"found": true, "entry": entry}), None),
            None => ok(json!({"found": false, "entry": null}), None),
        },
        request::LibraryRequest::Random { kind, count } => {
            let mut entries = db.list_entries().await?;
            if kind != "all" {
                entries.retain(|e| e.kind == kind);
            }
            let mut rng = rand::thread_rng();
            entries.shuffle(&mut rng);
            let results: Vec<&Entry> = entries.iter().take(count).collect();
            ok(json!({"results": results}), None)
        }
        request::LibraryRequest::Recent { kind, limit } => {
            let mut entries = db.list_entries().await?;
            if kind != "all" {
                entries.retain(|e| e.kind == kind);
            }
            entries.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
            let results: Vec<&Entry> = entries.iter().take(limit).collect();
            ok(json!({"results": results}), None)
        }
        request::LibraryRequest::Stats => {
            let entries = db.list_entries().await?;
            let total = entries.len();
            let mut by_kind = std::collections::BTreeMap::new();
            for e in &entries {
                *by_kind.entry(e.kind.clone()).or_insert(0) += 1;
            }
            let last_scan_ms = db.get_last_scan().await?;
            ok(
                json!({"total": total, "by_kind": by_kind, "last_scan_ms": last_scan_ms}),
                None,
            )
        }
    }
}

fn ok(data: Value, event: Option<(String, Value)>) -> Result<ActionResult, String> {
    let data = serde_json::to_vec(&data).map_err(|e| format!("failed to encode response: {e}"))?;
    Ok(ActionResult { data, event })
}
