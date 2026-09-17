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
const TOOLS_COLLECTION: &str = "agent-tools";
const EMBEDDING_FILTER_ENV: &str = "AGENT_PLUGIN_EMBEDDING_FILTER";

#[allow(clippy::match_like_matches_macro)]
fn embedding_filter_enabled() -> bool {
    if cfg!(test) {
        return false;
    }
    matches!(
        std::env::var(EMBEDDING_FILTER_ENV)
            .unwrap_or_else(|_| "off".into())
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "on" | "true" | "1" | "yes"
    )
}

/// Sentinel doc id holding a hash of the catalog's exact (name, description)
/// content. Never matches a real tool name, so it's harmless if it ever
/// surfaces in a `vec_query` result — [`embedding_filtered_catalog`] only
/// keeps hits present in `catalog.tools`.
const CATALOG_HASH_ID: &str = "__catalog_hash__";

/// Hash every tool's exact embedded text (name + description), order-
/// independent, so renaming/adding/removing a tool OR editing an existing
/// tool's description (without changing the count) both invalidate the
/// cache — unlike the old `stats.count`-within-5 heuristic, which missed
/// small catalog edits entirely (see BUG: tg_get_unread/tg_transcribe_voice
/// silently invisible to the LLM after being added to the allowlist).
fn catalog_content_hash(catalog: &Catalog) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut texts: Vec<String> =
        catalog.tools.iter().map(|t| format!("{} — {}", t.name, t.description)).collect();
    texts.sort_unstable();
    let mut hasher = DefaultHasher::new();
    texts.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

async fn ensure_tool_embeddings(rpc: &Rpc, catalog: &Catalog) -> Result<(), String> {
    let expected_hash = catalog_content_hash(catalog);
    let cached_hash = rpc
        .call("vec_get", json!({"collection": TOOLS_COLLECTION, "id": CATALOG_HASH_ID}), 5000)
        .await
        .ok()
        .filter(|v| v.get("found").and_then(Value::as_bool) == Some(true))
        .and_then(|v| v.get("metadata").and_then(|m| m.get("hash")).and_then(Value::as_str).map(str::to_string));
    if cached_hash.as_deref() == Some(expected_hash.as_str()) {
        return Ok(());
    }
    let docs: Vec<Value> = catalog
        .tools
        .iter()
        .map(|t| {
            let text = format!("{} — {}", t.name, t.description);
            json!({"id": t.name, "text": text, "metadata": {"name": t.name}})
        })
        .collect();
    for chunk in docs.chunks(50) {
        let batch = json!({"collection": TOOLS_COLLECTION, "docs": chunk});
        rpc.call("vec_upsert_batch", batch, 30000)
            .await
            .map_err(|e| format!("vec_upsert_batch failed: {e}"))?;
    }
    rpc.call(
        "vec_upsert",
        json!({
            "collection": TOOLS_COLLECTION,
            "id": CATALOG_HASH_ID,
            "text": CATALOG_HASH_ID,
            "metadata": {"hash": expected_hash},
        }),
        5000,
    )
    .await
    .map_err(|e| format!("vec_upsert (hash sentinel) failed: {e}"))?;
    Ok(())
}

async fn embedding_filtered_catalog(
    rpc: &Rpc,
    catalog: &Catalog,
    goal: &str,
) -> Option<Catalog> {
    if !embedding_filter_enabled() || goal.trim().is_empty() {
        return None;
    }
    if let Err(e) = ensure_tool_embeddings(rpc, catalog).await {
        eprintln!("[agent] embedding filter: ensure failed: {e}");
        return None;
    }
    let resp = rpc
        .call(
            "vec_query",
            json!({"collection": TOOLS_COLLECTION, "text": goal, "top_k": 35}),
            10000,
        )
        .await;
    let results = match resp {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[agent] embedding vec_query failed: {e}");
            return None;
        }
    };
    let hits: Vec<String> = results
        .get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|o| {
                    let score = o.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
                    if score < 0.35 {
                        return None;
                    }
                    o.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    if hits.len() < 3 {
        return None;
    }
    let hit_set: std::collections::HashSet<String> = hits.into_iter().collect();
    let filtered: Vec<_> = catalog
        .tools
        .iter()
        .filter(|t| hit_set.contains(&t.name))
        .cloned()
        .collect();
    if filtered.len() < 3 {
        return None;
    }
    Some(Catalog {
        tools: filtered,
        allowed_actions: catalog.allowed_actions.clone(),
        tools_file_set: catalog.tools_file_set,
    })
}

async fn effective_catalog(rpc: &Rpc, catalog: &Catalog, goal: &str, context: &str) -> Catalog {
    if let Some(emb) = embedding_filtered_catalog(rpc, catalog, goal).await {
        eprintln!(
            "[agent] embedding filter: {} -> {} tools (goal: {})",
            catalog.tools.len(),
            emb.tools.len(),
            &goal[..goal.len().min(60)]
        );
        return emb;
    }
    llm::filtered_catalog(catalog, goal, context)
}

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
    let effective = effective_catalog(rpc, catalog, &doc.goal, &doc.context).await;
    let llm_catalog = &effective;
    let dispatch_catalog = catalog;
    match entry {
        Entry::Fresh => {
            doc.status = store::STATUS_RUNNING.to_string();
            // A fresh doc is never degraded yet, so this must (and does)
            // compute the exact same answer the loop's first iteration will
            // reach via `want_native_tools` below — the seed and the first
            // `tools` param are always in sync.
            let native_seed = llm::want_native_tools(llm_catalog, doc.native_tools_disabled);
            doc.transcript =
                llm::opening_messages_with_full(&doc.goal, &doc.context, llm_catalog, dispatch_catalog, native_seed);
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

        let want_native = llm::want_native_tools(llm_catalog, doc.native_tools_disabled);
        let tools = if want_native {
            llm::catalog_tools_param(llm_catalog)
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
                // The seed never carried the text catalog (it went out
                // native-only) — inject it now, once, before the text-only
                // retry, so the model isn't left with neither channel
                // informed for the rest of the goal.
                push_turn(doc, "user", llm::degraded_tool_catalog_block(llm_catalog))?;
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

    fn spec(name: &str, description: &str) -> crate::tools::ToolSpec {
        crate::tools::ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            parameters: Value::Null,
            requires_confirmation: false,
            risk: String::new(),
            timeout_ms: 1000,
            cooldown_ms: 0,
            max_per_goal: 16,
            source: crate::tools::Source::Minimal,
        }
    }

    fn cat(tools: Vec<crate::tools::ToolSpec>) -> Catalog {
        Catalog { tools, allowed_actions: vec![], tools_file_set: false }
    }

    #[test]
    fn catalog_hash_is_order_independent() {
        let a = cat(vec![spec("a", "does a"), spec("b", "does b")]);
        let b = cat(vec![spec("b", "does b"), spec("a", "does a")]);
        assert_eq!(catalog_content_hash(&a), catalog_content_hash(&b));
    }

    #[test]
    fn catalog_hash_changes_when_a_tool_is_added() {
        let before = cat(vec![spec("a", "does a")]);
        let after = cat(vec![spec("a", "does a"), spec("b", "does b")]);
        assert_ne!(catalog_content_hash(&before), catalog_content_hash(&after));
    }

    #[test]
    fn catalog_hash_changes_when_a_description_is_edited_without_changing_count() {
        // Regression: the old count-within-5 heuristic missed exactly this
        // case — same number of tools, different content.
        let before = cat(vec![spec("a", "old description")]);
        let after = cat(vec![spec("a", "new description")]);
        assert_ne!(catalog_content_hash(&before), catalog_content_hash(&after));
    }

    #[test]
    fn catalog_hash_stable_for_identical_content() {
        let a = cat(vec![spec("a", "does a"), spec("b", "does b")]);
        let b = cat(vec![spec("a", "does a"), spec("b", "does b")]);
        assert_eq!(catalog_content_hash(&a), catalog_content_hash(&b));
    }
}
