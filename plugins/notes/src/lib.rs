//! `notes` plugin library crate: action dispatch over the `database` plugin.
//!
//! Thin schema layer (root `ROADMAP.md`): every note is one JSON document
//! stored under `note:<id>` in this plugin's own `database` namespace, ids
//! come from an atomic `db_incr` counter (`meta:next_id`), listing is a
//! `db_keys` prefix scan + `db_batch_get`. No local state — restart-safe by
//! construction.
//!
//! Outbound calls go through [`Rpc`], a channel-fronted proxy: handler tasks
//! never touch the `VynkorClient` directly, because `send_action` discards
//! every non-matching inbound frame while it waits — that is only safe from
//! a task owning ALL of the connection's traffic. With the proxy the serve
//! loop stays the single reader: user requests, pings and publish-acks are
//! always matched and routed, nothing is dropped (same rationale as
//! sync-client's custom loop).

pub mod request;
pub mod store;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use store::Note;

/// Runtime configuration (environment-driven; see `config.example.yaml`).
#[derive(Debug, Clone)]
pub struct Config {
    /// Per-call timeout for `database` IPC round-trips.
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
        let db_timeout_ms = std::env::var("NOTES_PLUGIN_DB_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5000);
        Self { db_timeout_ms }
    }
}

/// One pending kernel-routed call handed from a handler task to the serve
/// loop, which sends it and correlates the `ActionResponse` by `action_id`.
pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// Cloneable handle for kernel-routed actions into other plugins
/// (`database` today). Every [`Rpc::call`] round-trips through the serve
/// loop's single `recv()` point.
#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self { tx }
    }

    /// One kernel-routed action round-trip. Resolves to the decoded
    /// `data_json` payload on `ACTION_OK`; transport failures, non-OK
    /// statuses and timeouts all surface as `Err` naming the target action.
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
            .map_err(|_| format!("database.{action} aborted: serve loop is shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!(
                "database.{action} aborted: serve loop is shutting down"
            )),
            Err(_) => Err(format!("database.{action} timed out after {effective} ms")),
        }
    }
}

/// Best-effort change event to publish AFTER the action response is sent
/// (the kernel namespaces the type to `plugin.notes.changed`).
#[derive(Debug)]
pub struct ChangeEvent {
    pub event_type: &'static str,
    pub payload: Value,
}

/// One handled action: the response payload plus an optional change event.
#[derive(Debug)]
pub struct ActionResult {
    pub data: Vec<u8>,
    pub event: Option<ChangeEvent>,
}

/// Handle one kernel-routed action. Storage failures (including "not found"
/// on update) surface as `Err` → `ACTION_ERROR`; reading a missing note is
/// a `{"found": false}` result, not an error. Deleting a missing note is
/// `{"deleted": false}`, also not an error (idempotent).
pub async fn handle_action(
    rpc: Rpc,
    config: &Config,
    action: &str,
    params_json: &[u8],
) -> Result<ActionResult, String> {
    let req = request::parse_request(action, params_json)?;
    let db = store::Db::new(rpc, config.db_timeout_ms);
    match req {
        request::NotesRequest::Create { title, body, tags } => {
            let id = db.next_id().await?.to_string();
            let now = store::now_ms();
            let note = Note {
                id: id.clone(),
                title,
                body,
                tags,
                created_at_ms: now,
                updated_at_ms: now,
            };
            db.put(&note).await?;
            ok(
                json!({"id": id, "note": note}),
                Some(changed("created", &note.id)),
            )
        }
        request::NotesRequest::Get { id } => match db.get(&id).await? {
            Some(note) => ok(json!({"found": true, "note": note}), None),
            None => ok(json!({"found": false, "note": null}), None),
        },
        request::NotesRequest::List { tag, limit, offset } => {
            let mut notes = db.list().await?;
            if let Some(tag) = tag {
                notes.retain(|n| n.tags.iter().any(|t| t == &tag));
            }
            // Newest first; deterministic tie-break on numeric id desc so
            // same-millisecond mutations keep a stable order.
            notes.sort_by(|a, b| {
                b.updated_at_ms
                    .cmp(&a.updated_at_ms)
                    .then_with(|| id_num(b).cmp(&id_num(a)))
            });
            let total = notes.len();
            let page: Vec<&Note> = notes.iter().skip(offset).take(limit).collect();
            ok(json!({"notes": page, "total": total}), None)
        }
        request::NotesRequest::Update {
            id,
            title,
            body,
            tags,
        } => {
            let mut note = db
                .get(&id)
                .await?
                .ok_or_else(|| format!("note not found: {id}"))?;
            if let Some(t) = title {
                note.title = t;
            }
            if let Some(b) = body {
                note.body = b;
            }
            if let Some(t) = tags {
                note.tags = t;
            }
            if note.title.trim().is_empty() && note.body.trim().is_empty() {
                return Err("note must keep a non-empty title or body".to_string());
            }
            note.updated_at_ms = store::now_ms();
            db.put(&note).await?;
            ok(
                json!({"updated": true, "note": note}),
                Some(changed("updated", &note.id)),
            )
        }
        request::NotesRequest::Delete { id } => {
            let deleted = db.delete(&id).await?;
            let event = deleted.then(|| changed("deleted", &id));
            ok(json!({"deleted": deleted}), event)
        }
    }
}

fn id_num(n: &Note) -> u64 {
    n.id.parse::<u64>().unwrap_or(0)
}

fn ok(data: Value, event: Option<ChangeEvent>) -> Result<ActionResult, String> {
    let data = serde_json::to_vec(&data).map_err(|e| format!("failed to encode response: {e}"))?;
    Ok(ActionResult { data, event })
}

fn changed(op: &'static str, id: &str) -> ChangeEvent {
    ChangeEvent {
        event_type: "changed",
        payload: json!({"op": op, "id": id}),
    }
}
