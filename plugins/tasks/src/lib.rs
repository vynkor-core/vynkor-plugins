//! `tasks` plugin — task CRUD over `database`.

pub mod request;
pub mod store;

use serde_json::{json, Value};
use store::Task;
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
        let db_timeout_ms = std::env::var("TASKS_PLUGIN_DB_TIMEOUT_MS")
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
            .map_err(|_| format!("database.{action} aborted: shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!("database.{action} aborted: shutting down")),
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
        request::TasksRequest::Create {
            title,
            notes,
            list,
            due_ms,
            tags,
        } => {
            let id = db.next_id().await?.to_string();
            let now = store::now_ms();
            let task = Task {
                id: id.clone(),
                title,
                notes,
                list,
                tags,
                done: false,
                due_ms,
                created_at_ms: now,
                updated_at_ms: now,
                done_at_ms: None,
            };
            db.put(&task).await?;
            ok(
                json!({"id": id, "task": task}),
                Some(changed("created", &task.id)),
            )
        }
        request::TasksRequest::Get { id } => match db.get(&id).await? {
            Some(t) => ok(json!({"found": true, "task": t}), None),
            None => ok(json!({"found": false, "task": Value::Null}), None),
        },
        request::TasksRequest::List {
            query,
            list,
            status,
            tag,
            limit,
            offset,
        } => {
            let mut tasks = db.list().await?;
            if let Some(list) = list {
                tasks.retain(|t| t.list == list);
            }
            if let Some(status) = status {
                if status != "all" {
                    let want_done = status == "done";
                    tasks.retain(|t| t.done == want_done);
                }
            }
            if let Some(tag) = tag {
                tasks.retain(|t| t.tags.iter().any(|x| x == &tag));
            }
            if let Some(q) = query {
                let ql = q.to_lowercase();
                tasks.retain(|t| {
                    t.title.to_lowercase().contains(&ql) || t.notes.to_lowercase().contains(&ql)
                });
            }
            tasks.sort_by(|a, b| {
                b.updated_at_ms
                    .cmp(&a.updated_at_ms)
                    .then_with(|| id_num(b).cmp(&id_num(a)))
            });
            let total = tasks.len();
            let page: Vec<&Task> = tasks.iter().skip(offset).take(limit).collect();
            ok(json!({"tasks": page, "total": total}), None)
        }
        request::TasksRequest::Update {
            id,
            title,
            notes,
            list,
            due_ms,
            tags,
            done,
        } => {
            let mut t = db
                .get(&id)
                .await?
                .ok_or_else(|| format!("task not found: {id}"))?;
            if let Some(v) = title {
                t.title = v;
            }
            if let Some(v) = notes {
                t.notes = v;
            }
            if let Some(v) = list {
                t.list = v;
            }
            if let Some(v) = due_ms {
                t.due_ms = v;
            }
            if let Some(v) = tags {
                t.tags = v;
            }
            if let Some(d) = done {
                if t.done != d {
                    t.done = d;
                    t.done_at_ms = if d { Some(store::now_ms()) } else { None };
                }
            }
            t.updated_at_ms = store::now_ms();
            db.put(&t).await?;
            ok(
                json!({"updated": true, "task": t}),
                Some(changed("updated", &t.id)),
            )
        }
        request::TasksRequest::Done { id, done } => {
            let mut t = db
                .get(&id)
                .await?
                .ok_or_else(|| format!("task not found: {id}"))?;
            if t.done != done {
                t.done = done;
                t.done_at_ms = if done { Some(store::now_ms()) } else { None };
                t.updated_at_ms = store::now_ms();
                db.put(&t).await?;
            }
            let op = if done { "completed" } else { "reopened" };
            ok(json!({"done": t.done, "task": t}), Some(changed(op, &id)))
        }
        request::TasksRequest::Delete { id } => {
            let deleted = db.delete(&id).await?;
            let ev = deleted.then(|| changed("deleted", &id));
            ok(json!({"deleted": deleted}), ev)
        }
    }
}

fn id_num(t: &Task) -> u64 {
    t.id.parse::<u64>().unwrap_or(0)
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
