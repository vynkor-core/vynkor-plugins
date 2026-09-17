//! Typed access to the `database` plugin on behalf of `agent`.
//!
//! Same contract as `notes`/`calendar`: kernel-routed actions only via the
//! [`Rpc`] proxy, private namespace stamped by the kernel's per-caller
//! isolation. Every goal is one JSON document under `goal:<id>`; ids come
//! from an atomic `db_incr` counter (`meta:next_id`). No local state —
//! restart-safe by construction.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Rpc;

/// Counter key backing goal ids (atomic via `db_incr`).
pub const NEXT_ID_KEY: &str = "meta:next_id";
/// Key prefix for goal documents: `goal:<id>` → JSON [`GoalDoc`].
pub const KEY_PREFIX: &str = "goal:";

pub const STATUS_RUNNING: &str = "running";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_NEEDS_CONFIRMATION: &str = "needs_confirmation";
pub const STATUS_DECLINED: &str = "declined";
pub const STATUS_MAX_STEPS: &str = "max_steps_reached";
pub const STATUS_ERROR: &str = "error";

/// One message of the model conversation, persisted so a halted goal can be
/// resumed with full context later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub content: String,
}

/// The LLM routing plan snapshot taken at goal start — replayed unchanged by
/// `goal_resume` so an approved continuation uses the same provider/model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmPlan {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    #[serde(default)]
    pub agent_id: String,
    pub max_tokens: u32,
}

/// One line of the human-facing step log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRec {
    pub n: u32,
    /// `tool_ok` | `tool_error` | `unknown_tool` | `halt_confirm` |
    /// `final` | `max_steps` | `error`
    pub kind: String,
    #[serde(default)]
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalDoc {
    pub id: String,
    pub title: String,
    pub goal: String,
    #[serde(default)]
    pub context: String,
    /// One of the `STATUS_*` constants.
    pub status: String,
    #[serde(default)]
    pub final_answer: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub steps: Vec<StepRec>,
    #[serde(default)]
    pub transcript: Vec<Turn>,
    #[serde(default)]
    pub pending_tool: String,
    #[serde(default)]
    pub pending_params: Value,
    /// Set when a provider rejected the native `tools` param and the goal
    /// degraded to the text protocol mid-flight; later steps skip it.
    #[serde(default)]
    pub native_tools_disabled: bool,
    pub llm: LlmPlan,
    pub max_steps: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(default)]
    pub tool_counts: std::collections::BTreeMap<String, u32>,
    #[serde(default)]
    pub tool_last_ms: std::collections::BTreeMap<String, i64>,
}

impl GoalDoc {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            STATUS_COMPLETED | STATUS_DECLINED | STATUS_MAX_STEPS | STATUS_ERROR
        )
    }

    /// Light per-goal projection for `goal_list` — everything an operator
    /// needs to pick a goal to inspect, never the `transcript` and never the
    /// full `steps` array (those two fields are what make a goal doc big
    /// enough to blow `db_batch_get`'s response cap at realistic limits).
    /// `goal_get` still returns the full document.
    pub fn to_summary(&self) -> GoalSummary {
        GoalSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            goal: self.goal.clone(),
            status: self.status.clone(),
            final_answer: self.final_answer.clone(),
            error: self.error.clone(),
            pending_tool: self.pending_tool.clone(),
            step_count: self.steps.len(),
            max_steps: self.max_steps,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

/// `goal_list` response shape: id, status, goal text, timestamps, step
/// count, final_answer, error — no `transcript`, no `steps` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalSummary {
    pub id: String,
    pub title: String,
    pub goal: String,
    pub status: String,
    pub final_answer: String,
    pub error: String,
    pub pending_tool: String,
    pub step_count: usize,
    pub max_steps: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Typed wrapper over the `database` actions used by agent.
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

    /// Next monotonic goal id (atomic counter in our own namespace).
    pub async fn next_id(&self) -> Result<u64, String> {
        let v = self.call("db_incr", serde_json::json!({"key": NEXT_ID_KEY})).await?;
        v.get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("database.db_incr returned unexpected payload: {v}"))
    }

    pub async fn put(&self, doc: &GoalDoc) -> Result<(), String> {
        let key = format!("{KEY_PREFIX}{}", doc.id);
        let v = self.call("db_set", serde_json::json!({"key": key, "value": doc})).await?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("database.db_set returned unexpected payload: {v}"));
        }
        Ok(())
    }

    /// Missing goals read as `None`; a present-but-corrupt document is an
    /// error (loudness over silent data loss on single-doc reads).
    pub async fn get(&self, id: &str) -> Result<Option<GoalDoc>, String> {
        let v =
            self.call("db_get", serde_json::json!({"key": format!("{KEY_PREFIX}{id}")})).await?;
        if v.get("found").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let value = v.get("value").cloned().unwrap_or(Value::Null);
        let doc: GoalDoc = serde_json::from_value(value)
            .map_err(|e| format!("stored goal \"{id}\" is corrupt: {e}"))?;
        Ok(Some(doc))
    }

    /// All stored goal keys, newest first (by numeric id suffix).
    async fn sorted_keys(&self) -> Result<Vec<String>, String> {
        let v = self.call("db_keys", serde_json::json!({"prefix": KEY_PREFIX})).await?;
        let mut keys: Vec<String> = v
            .get("keys")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter().filter_map(|k| k.as_str()).map(str::to_string).collect::<Vec<_>>()
            })
            .ok_or_else(|| format!("database.db_keys returned unexpected payload: {v}"))?;
        keys.sort_by_key(|k| std::cmp::Reverse(key_num(k)));
        Ok(keys)
    }

    /// Keys per `db_batch_get` call — small enough that a batch of full
    /// goal docs (transcript + step log included) stays under database's
    /// `max_response_bytes` cap even at the worst-case doc size seen in
    /// production.
    const BATCH_CHUNK: usize = 20;

    /// Fetch and project the given keys to [`GoalSummary`], `BATCH_CHUNK`
    /// keys at a time. This bounds the per-call payload but, unlike a
    /// server-side projection, still moves the full document over the wire
    /// before this process throws the bulk of it away — see the module doc
    /// for what that does and doesn't fix.
    async fn fetch_summaries(&self, keys: &[String]) -> Result<Vec<GoalSummary>, String> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(Self::BATCH_CHUNK) {
            let batch = self.call("db_batch_get", serde_json::json!({"keys": chunk})).await?;
            let values = batch.get("values").and_then(Value::as_object).ok_or_else(|| {
                format!("database.db_batch_get returned unexpected payload: {batch}")
            })?;
            for key in chunk {
                let value = values.get(key).cloned().unwrap_or(Value::Null);
                let doc: GoalDoc = serde_json::from_value(value)
                    .map_err(|e| format!("stored goal \"{key}\" is corrupt: {e}"))?;
                out.push(doc.to_summary());
            }
        }
        Ok(out)
    }

    /// Newest first, light projection (no transcript, no full steps array).
    /// A corrupt entry fails the whole listing loudly rather than silently
    /// vanishing.
    pub async fn list_summaries(&self, limit: usize) -> Result<Vec<GoalSummary>, String> {
        let mut keys = self.sorted_keys().await?;
        keys.truncate(limit);
        self.fetch_summaries(&keys).await
    }

    /// Delete goal docs beyond `limit` (newest-first), never touching a
    /// goal that is still running or awaiting confirmation. `limit: None`
    /// (env unset/empty) is a no-op — checked before any key is fetched, so
    /// an operator who never opts in pays nothing extra. Returns the number
    /// of docs actually deleted.
    pub async fn prune(&self, limit: Option<usize>) -> Result<usize, String> {
        let Some(limit) = limit else { return Ok(0) };
        let keys = self.sorted_keys().await?;
        if keys.len() <= limit {
            return Ok(0);
        }
        let summaries = self.fetch_summaries(&keys).await?;
        let candidates = summaries
            .into_iter()
            .map(|s| PruneCandidate { id: s.id, status: s.status, updated_at_ms: s.updated_at_ms })
            .collect();
        let victims = prune_ids_at(Some(limit), candidates);
        let mut deleted = 0usize;
        for id in &victims {
            let key = format!("{KEY_PREFIX}{id}");
            let v = self.call("db_delete", serde_json::json!({"key": key})).await?;
            if v.get("ok").and_then(Value::as_bool) == Some(true) {
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}

fn key_num(key: &str) -> u64 {
    key.strip_prefix(KEY_PREFIX).and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// Operator env var: max goal docs to retain. Unset/empty means unlimited —
/// no behavior change for an operator who does not opt in. Never prunes a
/// goal that is still `running` or `needs_confirmation`, regardless of the
/// limit, so this can undershoot the cap when a lot of goals are in flight.
pub const RETENTION_LIMIT_ENV: &str = "AGENT_PLUGIN_GOAL_RETENTION_LIMIT";

/// Thin env wrapper — not exercised directly by tests (see [`prune_ids_at`]).
pub fn retention_limit() -> Option<usize> {
    std::env::var(RETENTION_LIMIT_ENV).ok().and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse().ok()
        }
    })
}

/// The slice of a stored goal that pruning decisions need.
#[derive(Debug, Clone, PartialEq)]
pub struct PruneCandidate {
    pub id: String,
    pub status: String,
    pub updated_at_ms: i64,
}

/// Pure core of retention: given every goal and a cap, return the ids to
/// delete. Keeps the `limit` most-recently-updated goals; anything older is
/// a deletion candidate UNLESS it is still running or awaiting confirmation
/// — those are never pruned, even if that means the store stays above
/// `limit` until the goal reaches a terminal status. `limit: None` (env
/// unset/empty) always returns no victims.
pub fn prune_ids_at(limit: Option<usize>, mut docs: Vec<PruneCandidate>) -> Vec<String> {
    let Some(limit) = limit else { return Vec::new() };
    if docs.len() <= limit {
        return Vec::new();
    }
    docs.sort_by_key(|d| std::cmp::Reverse(d.updated_at_ms));
    docs.into_iter()
        .skip(limit)
        .filter(|d| {
            matches!(
                d.status.as_str(),
                STATUS_COMPLETED | STATUS_DECLINED | STATUS_MAX_STEPS | STATUS_ERROR
            )
        })
        .map(|d| d.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_num_sorts_numerically() {
        assert_eq!(key_num("goal:12"), 12);
        assert_eq!(key_num("goal:x"), 0);
        let mut keys = vec!["goal:10", "goal:9", "goal:2"];
        keys.sort_by_key(|k| std::cmp::Reverse(key_num(k)));
        assert_eq!(keys, vec!["goal:10", "goal:9", "goal:2"]);
    }

    #[test]
    fn terminal_statuses_are_exact() {
        let mut doc = sample(STATUS_RUNNING);
        assert!(!doc.is_terminal());
        for s in [STATUS_COMPLETED, STATUS_DECLINED, STATUS_MAX_STEPS, STATUS_ERROR] {
            doc.status = s.to_string();
            assert!(doc.is_terminal(), "{s}");
        }
        assert!(!sample(STATUS_NEEDS_CONFIRMATION).is_terminal());
    }

    fn sample(status: &str) -> GoalDoc {
        GoalDoc {
            id: "1".into(),
            title: "t".into(),
            goal: "g".into(),
            context: String::new(),
            status: status.into(),
            final_answer: String::new(),
            error: String::new(),
            steps: Vec::new(),
            transcript: Vec::new(),
            pending_tool: String::new(),
            pending_params: Value::Null,
            native_tools_disabled: false,
            llm: LlmPlan {
                provider: "openai".into(),
                base_url: String::new(),
                model: "m".into(),
                api_key_env: "K".into(),
                agent_id: String::new(),
                max_tokens: 1024,
            },
            max_steps: 6,
            created_at_ms: 0,
            updated_at_ms: 0,
            tool_counts: Default::default(),
            tool_last_ms: Default::default(),
        }
    }

    #[test]
    fn to_summary_excludes_transcript_and_full_steps() {
        let mut doc = sample(STATUS_COMPLETED);
        doc.transcript = vec![
            Turn { role: "user".into(), content: "a very long goal description".into() },
            Turn { role: "assistant".into(), content: "a very long reply".into() },
        ];
        doc.steps = vec![
            StepRec { n: 1, kind: "tool_ok".into(), detail: serde_json::json!({"tool": "x"}) },
            StepRec { n: 2, kind: "final".into(), detail: Value::Null },
        ];
        doc.final_answer = "done".into();
        doc.error = "".into();

        let summary = doc.to_summary();
        assert_eq!(summary.id, "1");
        assert_eq!(summary.status, STATUS_COMPLETED);
        assert_eq!(summary.goal, "g");
        assert_eq!(summary.step_count, 2);
        assert_eq!(summary.final_answer, "done");

        // The struct has no transcript/steps fields at all, so the
        // serialized JSON can never carry them — assert that directly on
        // the wire shape, which is what a caller actually receives.
        let json = serde_json::to_value(&summary).unwrap();
        assert!(json.get("transcript").is_none(), "{json}");
        assert!(json.get("steps").is_none(), "{json}");
        assert_eq!(json["step_count"], 2);
    }

    #[test]
    fn prune_ids_unlimited_when_env_unset() {
        let docs = vec![
            PruneCandidate { id: "1".into(), status: STATUS_COMPLETED.into(), updated_at_ms: 1 },
            PruneCandidate { id: "2".into(), status: STATUS_COMPLETED.into(), updated_at_ms: 2 },
        ];
        assert_eq!(prune_ids_at(None, docs), Vec::<String>::new());
    }

    #[test]
    fn prune_ids_noop_when_within_limit() {
        let docs = vec![
            PruneCandidate { id: "1".into(), status: STATUS_COMPLETED.into(), updated_at_ms: 1 },
            PruneCandidate { id: "2".into(), status: STATUS_COMPLETED.into(), updated_at_ms: 2 },
        ];
        assert_eq!(prune_ids_at(Some(5), docs), Vec::<String>::new());
    }

    #[test]
    fn prune_ids_deletes_oldest_terminal_beyond_limit() {
        let docs = vec![
            PruneCandidate { id: "old".into(), status: STATUS_COMPLETED.into(), updated_at_ms: 1 },
            PruneCandidate { id: "mid".into(), status: STATUS_ERROR.into(), updated_at_ms: 2 },
            PruneCandidate { id: "new".into(), status: STATUS_COMPLETED.into(), updated_at_ms: 3 },
        ];
        let victims = prune_ids_at(Some(2), docs);
        assert_eq!(victims, vec!["old".to_string()]);
    }

    #[test]
    fn prune_ids_never_touches_running_or_needs_confirmation() {
        let docs = vec![
            PruneCandidate { id: "running".into(), status: STATUS_RUNNING.into(), updated_at_ms: 1 },
            PruneCandidate {
                id: "confirm".into(),
                status: STATUS_NEEDS_CONFIRMATION.into(),
                updated_at_ms: 2,
            },
            PruneCandidate { id: "new".into(), status: STATUS_COMPLETED.into(), updated_at_ms: 3 },
        ];
        // Limit of 1 would normally evict both "running" and "confirm" —
        // neither may ever be deleted, so the victim list must be empty.
        let victims = prune_ids_at(Some(1), docs);
        assert_eq!(victims, Vec::<String>::new());
    }
}
