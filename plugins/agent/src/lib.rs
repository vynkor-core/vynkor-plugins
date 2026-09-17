//! `agent` plugin library crate: the multi-step goal loop over `ai`'s
//! `chat_completion`, dispatching other plugins' kernel-routed actions as
//! tools.
//!
//! Thin-wrapper shape (root `ROADMAP.md`): no business primitives of its
//! own — goals persist as JSON documents under `goal:<id>` in this plugin's
//! own `database` namespace, LLM traffic routes through `ai`, tool calls
//! route through the kernel to whichever plugin owns the action. The agent
//! declares `PERMISSION_STORAGE` + `PERMISSION_EVENT_PUBLISH`; permissions
//! for every *dispatched* action come from the operator's JWT grant (T-19:
//! the caller of a gated action must hold its permission itself).
//!
//! Outbound calls go through [`Rpc`], a channel-fronted proxy: handler
//! tasks never touch the `VynkorClient` directly, because `send_action`
//! discards every non-matching inbound frame while it waits — a long goal
//! loop would silently eat user requests arriving mid-run. With the proxy
//! the serve loop stays the single reader and nothing inbound is dropped
//! (`docs/PLUGIN_AUTHORING.md` §1).
//!
//! Tool safety: only names on the operator's allowlist are ever dispatched,
//! and confirmation-marked tools halt the goal until an approved resume —
//! see `src/tools.rs`.

pub mod discovery;
pub mod engine;
pub mod llm;
pub mod memory;
pub mod request;
pub mod store;
pub mod tools;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::CommandStatus;

use request::{AgentRequest, GoalStartParams};
use store::{Db, GoalDoc, LlmPlan};
use tools::Catalog;

/// Runtime configuration (environment-driven; see `config.example.yaml`).
#[derive(Debug, Clone)]
pub struct Config {
    /// Per-call timeout for `database` IPC round-trips.
    pub db_timeout_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self { db_timeout_ms: 5000 }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let db_timeout_ms = std::env::var("AGENT_PLUGIN_DB_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5000);
        Self { db_timeout_ms }
    }
}

/// Cloneable handle for kernel-routed actions into other plugins
/// (`database`, `ai`, every catalogued tool). Every [`Rpc::call`]
/// round-trips through the serve loop's single `recv()` point.
#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<ProxyMsg>,
    next_command_id: Arc<AtomicU64>,
}

/// One pending kernel-routed action handed from a task to the serve loop,
/// which sends it and correlates the `ActionResponse` by `action_id`.
pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// One pending kernel command (`list_plugins`, `get_manifest`, …) handed to
/// the serve loop; correlated by kernel-echoed `command_id`.
pub struct CommandCall {
    pub command_id: String,
    pub command: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

pub enum ProxyMsg {
    Action(RpcCall),
    Command(CommandCall),
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<ProxyMsg>) -> Self {
        Self { tx, next_command_id: Arc::new(AtomicU64::new(1)) }
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
            .send(ProxyMsg::Action(RpcCall { action: action.to_string(), params_json, timeout_ms, reply }))
            .await
            .map_err(|_| format!("{action} aborted: serve loop is shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("{action} aborted: serve loop is shutting down")),
            Err(_) => Err(format!("{action} timed out after {effective} ms")),
        }
    }

    /// One kernel command round-trip (`list_plugins`, `get_manifest`, …).
    /// Resolves to the decoded ack `data_json` on `COMMAND_OK`; any other
    /// status surfaces as `Err` carrying the ack's error text.
    pub async fn call_command(
        &self,
        command: &str,
        params: Value,
        timeout_ms: u32,
    ) -> Result<Value, String> {
        let n = self.next_command_id.fetch_add(1, Ordering::Relaxed);
        let params_json = serde_json::to_vec(&params)
            .map_err(|e| format!("failed to encode {command} params: {e}"))?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ProxyMsg::Command(CommandCall {
                command_id: format!("cmd-{n}"),
                command: command.to_string(),
                params_json,
                timeout_ms,
                reply,
            }))
            .await
            .map_err(|_| format!("{command} aborted: serve loop is shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("{command} aborted: serve loop is shutting down")),
            Err(_) => Err(format!("{command} timed out after {effective} ms")),
        }
    }
}

/// Decode a `KernelCommandAck` payload for [`Rpc::call_command`].
pub fn command_ack_result(
    status: i32,
    data_json: Vec<u8>,
    error: String,
) -> Result<Value, String> {
    if status == CommandStatus::CommandOk as i32 {
        serde_json::from_slice::<Value>(&data_json)
            .map_err(|e| format!("malformed command payload: {e}"))
    } else {
        Err(error)
    }
}

/// Best-effort change event to publish AFTER the action response is sent
/// (the kernel namespaces the type to `plugin.agent.changed`).
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

fn ok(data: Value, event: Option<ChangeEvent>) -> Result<ActionResult, String> {
    let data =
        serde_json::to_vec(&data).map_err(|e| format!("failed to encode response: {e}"))?;
    Ok(ActionResult { data, event })
}

fn changed(status: &str, id: &str) -> ChangeEvent {
    ChangeEvent { event_type: "changed", payload: json!({"op": status, "id": id}) }
}

/// Resolve one goal's LLM plan from env defaults + per-request overrides.
/// Explicit per-request fields win; an `agent_id` anywhere switches `ai`
/// into profile mode (provider/model/key resolved by `ai` itself).
fn build_plan(cfg: &GoalStartParams) -> LlmPlan {
    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let pick = |over: &Option<String>, env_key: &str| {
        over.clone().or_else(|| env(env_key)).unwrap_or_default()
    };
    LlmPlan {
        agent_id: cfg
            .llm
            .agent_id
            .clone()
            .or_else(|| env("AGENT_PLUGIN_AI_AGENT_ID"))
            .unwrap_or_default(),
        provider: pick(&cfg.llm.provider, "AGENT_PLUGIN_AI_PROVIDER"),
        base_url: pick(&cfg.llm.base_url, "AGENT_PLUGIN_AI_BASE_URL"),
        model: pick(&cfg.llm.model, "AGENT_PLUGIN_AI_MODEL"),
        api_key_env: pick(&cfg.llm.api_key_env, "AGENT_PLUGIN_AI_API_KEY_ENV"),
        max_tokens: cfg
            .llm
            .max_tokens
            .or_else(|| {
                env("AGENT_PLUGIN_AI_MAX_TOKENS").and_then(|s| s.parse().ok())
            })
            .unwrap_or(llm::DEFAULT_MAX_TOKENS),
    }
}

fn default_title(goal: &str) -> String {
    let mut t: String = goal.chars().take(60).collect();
    if goal.chars().count() > 60 {
        t.push('…');
    }
    t
}

async fn start_goal(db: &Db, rpc: &Rpc, params: GoalStartParams) -> Result<ActionResult, String> {
    let catalog = Catalog::load_with_discovery(rpc).await?;
    let id = db.next_id().await?.to_string();
    let now = store::now_ms();
    let llm = build_plan(&params);
    let mut doc = GoalDoc {
        id: id.clone(),
        title: params.title.clone().unwrap_or_else(|| default_title(&params.goal)),
        goal: params.goal,
        context: params.context,
        status: store::STATUS_RUNNING.to_string(),
        final_answer: String::new(),
        error: String::new(),
        steps: Vec::new(),
        transcript: Vec::new(),
        pending_tool: String::new(),
        pending_params: Value::Null,
        native_tools_disabled: false,
        llm,
        max_steps: if params.max_steps == 0 {
            std::env::var("AGENT_PLUGIN_MAX_STEPS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(6)
                .clamp(1, request::MAX_STEPS_CAP)
        } else {
            params.max_steps
        },
        created_at_ms: now,
        updated_at_ms: now,
        tool_counts: Default::default(),
        tool_last_ms: Default::default(),
    };

    engine::run(db, rpc, &catalog, &mut doc, engine::Entry::Fresh).await?;

    // Best-effort retention: bound the store so it doesn't grow forever.
    // No-op unless the operator opted in via AGENT_PLUGIN_GOAL_RETENTION_LIMIT
    // (store::retention_limit()); a prune failure never fails goal_start.
    if let Err(e) = db.prune(store::retention_limit()).await {
        eprintln!("[agent] goal retention prune failed: {e}");
    }

    ok(summary(&doc), Some(changed(&doc.status, &doc.id)))
}

async fn resume_goal(
    db: &Db,
    rpc: &Rpc,
    id: &str,
    approve: bool,
) -> Result<ActionResult, String> {
    let mut doc =
        db.get(id).await?.ok_or_else(|| format!("goal not found: {id}"))?;
    if doc.status != store::STATUS_NEEDS_CONFIRMATION {
        return Err(format!(
            "goal \"{id}\" is not awaiting confirmation (status: {})",
            doc.status
        ));
    }

    if !approve {
        let tool = doc.pending_tool.clone();
        doc.status = store::STATUS_DECLINED.to_string();
        doc.pending_tool.clear();
        doc.pending_params = Value::Null;
        doc.final_answer = format!("Operator declined the proposed call of \"{tool}\".");
        doc.steps.push(store::StepRec {
            n: doc.steps.iter().map(|s| s.n).max().unwrap_or(0) + 1,
            kind: "declined".to_string(),
            detail: json!({"tool": tool}),
        });
        engine::persist(db, &mut doc).await?;
        return ok(summary(&doc), Some(changed(&doc.status, &doc.id)));
    }

    let catalog = Catalog::load_with_discovery(rpc).await?;
    engine::run(db, rpc, &catalog, &mut doc, engine::Entry::ApprovedResume).await?;
    ok(summary(&doc), Some(changed(&doc.status, &doc.id)))
}

/// Response summary: everything except the bulky transcript.
fn summary(doc: &GoalDoc) -> Value {
    json!({
        "id": doc.id,
        "status": doc.status,
        "title": doc.title,
        "final_answer": doc.final_answer,
        "error": doc.error,
        "steps": doc.steps,
        "pending_tool": doc.pending_tool,
        "max_steps": doc.max_steps,
    })
}

/// Handle one kernel-routed action. Storage failures surface as `Err` →
/// `ACTION_ERROR`; reading a missing goal is a `{"found": false}` result —
/// resuming a missing one is an error (house rule: updates of missing
/// entities fail, reads don't).
pub async fn handle_action(
    rpc: Rpc,
    config: &Config,
    action: &str,
    params_json: &[u8],
) -> Result<ActionResult, String> {
    let req = request::parse_request(action, params_json)?;
    let db = Db::new(rpc.clone(), config.db_timeout_ms);
    match req {
        AgentRequest::GoalStart(params) => start_goal(&db, &rpc, params).await,
        AgentRequest::GoalGet { id } => match db.get(&id).await? {
            Some(doc) => ok(json!({"found": true, "goal": doc}), None),
            None => ok(json!({"found": false, "goal": null}), None),
        },
        AgentRequest::GoalList { limit } => {
            let goals = db.list_summaries(limit).await?;
            ok(json!({"total": goals.len(), "goals": goals}), None)
        }
        AgentRequest::GoalResume { id, approve } => resume_goal(&db, &rpc, &id, approve).await,
        AgentRequest::ToolsList => {
            let catalog = Catalog::load_with_discovery(&rpc).await?;
            ok(
                json!({
                    "tools": catalog.tools,
                    "allowed_actions": catalog.allowed_actions,
                    "tools_file_set": catalog.tools_file_set,
                }),
                None,
            )
        }
        AgentRequest::MemoryForget { query } => match memory::forget(&rpc, &query).await? {
            Some(id) => ok(json!({"forgotten": true, "id": id}), None),
            None => ok(json!({"forgotten": false, "id": null}), None),
        },
        AgentRequest::MemoryClear => {
            let deleted = memory::clear(&rpc).await?;
            ok(json!({"cleared": deleted}), None)
        }
        AgentRequest::MemoryList => {
            let facts = memory::list(&rpc).await?;
            ok(json!({"total": facts.len(), "facts": facts}), None)
        }
    }
}
