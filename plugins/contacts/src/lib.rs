//! `contacts` plugin — vCard-ish contact store over `database`.

pub mod request;
pub mod store;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use store::Contact;

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
        let db_timeout_ms = std::env::var("CONTACTS_PLUGIN_DB_TIMEOUT_MS")
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
            .map_err(|_| format!("database.{action} aborted: serve loop is shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!(
                "database.{action} aborted: serve loop is shutting down"
            )),
            Err(_) => Err(format!("database.{action} timed out after {effective} ms")),
        }
    }
}

#[derive(Debug)]
pub struct ChangeEvent {
    pub event_type: &'static str,
    pub payload: Value,
}
#[derive(Debug)]
pub struct ActionResult {
    pub data: Vec<u8>,
    pub event: Option<ChangeEvent>,
}

pub async fn handle_action(
    rpc: Rpc,
    config: &Config,
    action: &str,
    params_json: &[u8],
) -> Result<ActionResult, String> {
    let req = request::parse_request(action, params_json)?;
    let db = store::Db::new(rpc, config.db_timeout_ms);
    match req {
        request::ContactsRequest::Create {
            name,
            email,
            phone,
            notes,
            tags,
        } => {
            let id = db.next_id().await?.to_string();
            let now = store::now_ms();
            let contact = Contact {
                id: id.clone(),
                name,
                email,
                phone,
                notes,
                tags,
                created_at_ms: now,
                updated_at_ms: now,
            };
            db.put(&contact).await?;
            ok(
                json!({"id": id, "contact": contact}),
                Some(changed("created", &contact.id)),
            )
        }
        request::ContactsRequest::Get { id } => match db.get(&id).await? {
            Some(c) => ok(json!({"found": true, "contact": c}), None),
            None => ok(json!({"found": false, "contact": Value::Null}), None),
        },
        request::ContactsRequest::List {
            query,
            tag,
            limit,
            offset,
        } => {
            let mut contacts = db.list().await?;
            if let Some(tag) = tag {
                contacts.retain(|c| c.tags.iter().any(|t| t == &tag));
            }
            if let Some(q) = query {
                let ql = q.to_lowercase();
                contacts.retain(|c| {
                    c.name.to_lowercase().contains(&ql)
                        || c.email.to_lowercase().contains(&ql)
                        || c.phone.contains(&ql)
                        || c.notes.to_lowercase().contains(&ql)
                });
            }
            contacts.sort_by(|a, b| {
                b.updated_at_ms
                    .cmp(&a.updated_at_ms)
                    .then_with(|| id_num(b).cmp(&id_num(a)))
            });
            let total = contacts.len();
            let page: Vec<&Contact> = contacts.iter().skip(offset).take(limit).collect();
            ok(json!({"contacts": page, "total": total}), None)
        }
        request::ContactsRequest::Update {
            id,
            name,
            email,
            phone,
            notes,
            tags,
        } => {
            let mut c = db
                .get(&id)
                .await?
                .ok_or_else(|| format!("contact not found: {id}"))?;
            if let Some(v) = name {
                c.name = v;
            }
            if let Some(v) = email {
                c.email = v;
            }
            if let Some(v) = phone {
                c.phone = v;
            }
            if let Some(v) = notes {
                c.notes = v;
            }
            if let Some(v) = tags {
                c.tags = v;
            }
            c.updated_at_ms = store::now_ms();
            db.put(&c).await?;
            ok(
                json!({"updated": true, "contact": c}),
                Some(changed("updated", &c.id)),
            )
        }
        request::ContactsRequest::Delete { id } => {
            let deleted = db.delete(&id).await?;
            let ev = deleted.then(|| changed("deleted", &id));
            ok(json!({"deleted": deleted}), ev)
        }
    }
}

fn id_num(c: &Contact) -> u64 {
    c.id.parse::<u64>().unwrap_or(0)
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
