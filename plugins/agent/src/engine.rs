//! The goal loop: model reply → tool dispatch → observation → repeat,
//! bounded by `max_steps`, persisted to `database` after every mutation.
//!
//! Safety rails:
//! - only tools in the operator's [`Catalog`] are ever dispatched;
//! - tools marked `requires_confirmation` halt the loop in
//!   `needs_confirmation` instead of running (D-09 spirit — the human
//!   approves via `goal_resume`, the engine never self-confirms);
//! - tool results and the transcript are size-capped so one chatty plugin
//!   can't blow the context or the storage document.

use serde_json::{json, Value};

use crate::llm::{self, Reply};
use crate::memory;
use crate::store::{self, Db, GoalDoc, StepRec, Turn, STATUS_COMPLETED, STATUS_ERROR,
                   STATUS_MAX_STEPS, STATUS_NEEDS_CONFIRMATION};
use crate::tools::Catalog;
use crate::Rpc;

/// Cap on a single tool result fed back into the transcript (chars).
const OBSERVATION_MAX: usize = 8192;
/// Cap on the whole persisted transcript (chars) — guards both the LLM
/// context window and the `database` document size.
const TRANSCRIPT_MAX_CHARS: usize = 262_144;

pub enum Entry {
    /// Fresh goal: seed the transcript from goal + catalog.
    Fresh,
    /// Resumed after approval: dispatch the pending confirmation first.
    ApprovedResume,
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…[truncated]")
    }
}

fn push_turn(doc: &mut GoalDoc, role: &str, content: String) -> Result<(), String> {
    doc.transcript.push(Turn { role: role.to_string(), content });
    let total: usize = doc.transcript.iter().map(|t| t.content.chars().count()).sum();
    if total > TRANSCRIPT_MAX_CHARS {
        return Err(format!(
            "transcript budget exceeded ({total} > {TRANSCRIPT_MAX_CHARS} chars); \
             start a new goal with narrower scope"
        ));
    }
    Ok(())
}

pub(crate) async fn persist(db: &Db, doc: &mut GoalDoc) -> Result<(), String> {
    doc.updated_at_ms = store::now_ms();
    db.put(doc).await
}

fn next_step(doc: &mut GoalDoc, kind: &str, detail: Value) {
    let n = doc.steps.iter().map(|s| s.n).max().unwrap_or(0) + 1;
    doc.steps.push(StepRec { n, kind: kind.to_string(), detail });
}

fn is_retryable_device_error(err: &str) -> bool {
    err.contains("unknown target")
        || err.contains("not registered")
        || err.contains("timed out")
        || err.contains("no live connection")
}

fn is_device_target(name: &str) -> bool {
    name.starts_with("dev-") && name.contains('.')
}

/// Dispatch one tool call and append the observation turn.
async fn dispatch_and_observe(
    rpc: &Rpc,
    doc: &mut GoalDoc,
    spec_timeout_ms: u32,
    name: &str,
    params: Value,
) -> Result<bool, String> {
    let mut outcome = rpc.call(name, params.clone(), spec_timeout_ms).await;
    if outcome.is_err() && is_device_target(name) {
        let err = outcome.as_ref().err().unwrap().to_string();
        if is_retryable_device_error(&err) {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            outcome = rpc.call(name, params.clone(), spec_timeout_ms).await;
        }
    }
    let (status, body) = match outcome {
        Ok(v) => ("ok", serde_json::to_string(&v).unwrap_or_else(|_| "{}".into())),
        Err(e) => ("error", e),
    };
    let ok = status == "ok";
    let observation =
        truncate_chars(&format!("[TOOL RESULT name={name} status={status}]\n{body}"), OBSERVATION_MAX);
    push_turn(doc, "user", observation)?;
    next_step(
        doc,
        if ok { "tool_ok" } else { "tool_error" },
        json!({"tool": name, "params": params}),
    );
    Ok(ok)
}

fn check_quota(doc: &GoalDoc, spec: &crate::tools::ToolSpec) -> Result<(), String> {
    let count = doc.tool_counts.get(&spec.name).copied().unwrap_or(0);
    if spec.max_per_goal > 0 && count >= spec.max_per_goal {
        return Err(format!(
            "tool '{}' quota exceeded: max {} calls per goal (used {})",
            spec.name, spec.max_per_goal, count
        ));
    }
    if spec.cooldown_ms > 0 {
        if let Some(last) = doc.tool_last_ms.get(&spec.name) {
            let now = store::now_ms();
            let elapsed = now - *last;
            if elapsed < spec.cooldown_ms as i64 {
                return Err(format!(
                    "tool '{}' is on cooldown: {}ms remaining (cooldown {}ms)",
                    spec.name,
                    spec.cooldown_ms as i64 - elapsed,
                    spec.cooldown_ms
                ));
            }
        }
    }
    Ok(())
}

/// Run (or continue) one goal to a terminal or halting state. Storage
/// failures bubble as `Err`; every other outcome is recorded on the doc.
pub async fn run(
    db: &Db,
    rpc: &Rpc,
    catalog: &Catalog,
    doc: &mut GoalDoc,
    entry: Entry,
) -> Result<(), String> {
    match entry {
        Entry::Fresh => {
            doc.status = store::STATUS_RUNNING.to_string();
            doc.transcript = llm::opening_messages(&doc.goal, &doc.context, catalog);
            if memory::enabled() {
                if let Some(block) = memory::recall(rpc, &doc.goal).await {
                    push_turn(doc, "user", block)?;
                }
            }
            persist(db, doc).await?;
        }
        Entry::ApprovedResume => {
            let name = std::mem::take(&mut doc.pending_tool);
            let params = std::mem::replace(&mut doc.pending_params, Value::Null);
            let spec = catalog.get(&name);
            let timeout = spec.map(|s| s.timeout_ms).unwrap_or(30_000);
            doc.status = store::STATUS_RUNNING.to_string();
            if let Some(s) = spec {
                if let Err(e) = check_quota(doc, s) {
                    push_turn(doc, "user", format!("[TOOL RESULT name={} status=error]\n{e}", name))?;
                    next_step(doc, "tool_error", json!({"tool": name, "error": e}));
                    persist(db, doc).await?;
                    return Ok(());
                }
                doc.tool_counts.entry(name.clone()).and_modify(|c| *c += 1).or_insert(1);
                doc.tool_last_ms.insert(name.clone(), store::now_ms());
            }
            dispatch_and_observe(rpc, doc, timeout, &name, params).await?;
            persist(db, doc).await?;
        }
    }

    loop {
        if doc.steps.iter().filter(|s| s.kind != "final").count() >= doc.max_steps as usize {
            doc.status = STATUS_MAX_STEPS.to_string();
            next_step(doc, "max_steps", json!({"max_steps": doc.max_steps}));
            persist(db, doc).await?;
            return Ok(());
        }

        let want_native = match llm::native_mode() {
            llm::NativeMode::Off => false,
            llm::NativeMode::On => true,
            llm::NativeMode::Auto => !catalog.tools.is_empty(),
        } && !doc.native_tools_disabled;
        let tools = if want_native {
            llm::catalog_tools_param(catalog)
        } else {
            Vec::new()
        };

        let outcome = llm::chat_with_fallback(rpc, &doc.llm, &doc.transcript, &tools).await;
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) if want_native => {
                // Provider rejected the tools param (models without tool
                // support) — retry this turn text-only and stay degraded
                // for the rest of the goal instead of failing it.
                eprintln!("[agent] native tools rejected ({e}); degrading goal to text protocol");
                doc.native_tools_disabled = true;
                match llm::chat_with_fallback(rpc, &doc.llm, &doc.transcript, &[]).await {
                    Ok(o) => o,
                    Err(e2) => {
                        doc.status = STATUS_ERROR.to_string();
                        doc.error = e2.clone();
                        next_step(doc, "error", json!({"error": e2}));
                        persist(db, doc).await?;
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                doc.status = STATUS_ERROR.to_string();
                doc.error = e.clone();
                next_step(doc, "error", json!({"error": e}));
                persist(db, doc).await?;
                return Ok(());
            }
        };
        push_turn(doc, "assistant", outcome.content.clone())?;

        match llm::outcome_to_reply(&outcome) {
            Reply::Final(answer) => {
                doc.status = STATUS_COMPLETED.to_string();
                doc.final_answer = answer.clone();
                next_step(doc, "final", json!({}));
                persist(db, doc).await?;
                if memory::enabled() {
                    // Detached: extraction is one extra LLM round-trip and
                    // must not sit between the finished goal and its caller.
                    let rpc = rpc.clone();
                    let plan = doc.llm.clone();
                    let goal_id = doc.id.clone();
                    let goal = doc.goal.clone();
                    tokio::spawn(async move {
                        memory::remember(&rpc, &plan, &goal_id, &goal, &answer).await;
                    });
                }
                return Ok(());
            }
            Reply::ToolCall { name, params } => {
                let Some(spec) = catalog.get(&name) else {
                    let msg = format!(
                        "unknown tool \"{name}\": not in the operator's tool catalog"
                    );
                    push_turn(
                        doc,
                        "user",
                        format!("[TOOL RESULT name={name} status=error]\n{msg}"),
                    )?;
                    next_step(doc, "unknown_tool", json!({"tool": name}));
                    persist(db, doc).await?;
                    continue;
                };
                if spec.requires_confirmation {
                    doc.status = STATUS_NEEDS_CONFIRMATION.to_string();
                    doc.pending_tool = name.clone();
                    doc.pending_params = params.clone();
                    next_step(doc, "halt_confirm", json!({"tool": name, "params": params}));
                    persist(db, doc).await?;
                    return Ok(());
                }
                if let Err(quota_err) = check_quota(doc, spec) {
                    let msg = quota_err.clone();
                    push_turn(doc, "user", format!("[TOOL RESULT name={name} status=error]\n{msg}"))?;
                    next_step(doc, "tool_error", json!({"tool": name, "error": msg}));
                    persist(db, doc).await?;
                    continue;
                }
                doc.tool_counts.entry(name.clone()).and_modify(|c| *c += 1).or_insert(1);
                doc.tool_last_ms.insert(name.clone(), store::now_ms());
                dispatch_and_observe(rpc, doc, spec.timeout_ms, &name, params).await?;
                persist(db, doc).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("short", 10), "short");
        let long = "ж".repeat(20);
        let cut = truncate_chars(&long, 5);
        assert!(cut.starts_with("жжжжж"));
        assert!(cut.ends_with("[truncated]"));
    }
}
