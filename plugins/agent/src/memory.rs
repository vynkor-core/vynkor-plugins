//! Agent long-term memory (AGT-01) over the `vector-db` plugin.
//!
//! After a goal completes, an extraction pass turns `goal` + `final_answer`
//! into durable facts (a cheap extra `chat_completion`), embeds and stores
//! them via `vector-db`'s `vec_upsert` (which routes embedding through `ai`
//! itself), and indexes their ids in this plugin's own `database` namespace
//! under [`INDEX_KEY`]. Before a fresh goal runs, the top-K similar facts
//! are recalled into the transcript as a leading context turn.
//!
//! Opt-in by default (`AGENT_PLUGIN_MEMORY=on`) — memory writes are a
//! privacy decision. All state lives in the agent's per-caller vector-db /
//! database namespaces; `memory_clear` wipes it deterministically through
//! the id index (vector-db has no collection wipe primitive).

use serde_json::{json, Value};

use crate::llm;
#[allow(unused_imports)]
use crate::store::{self, Db, LlmPlan};
use crate::Rpc;

pub const MEMORY_ENV: &str = "AGENT_PLUGIN_MEMORY";
pub const COLLECTION_ENV: &str = "AGENT_PLUGIN_MEMORY_COLLECTION";
pub const DEFAULT_COLLECTION: &str = "agent-memory";
/// Index of fact ids in our own database namespace (drives clear/forget/list).
pub const INDEX_KEY: &str = "memory:index";
const MAX_FACTS_PER_GOAL: usize = 8;
const MAX_FACT_CHARS: usize = 512;
pub const RECALL_TOP_K: u32 = 5;
pub const MIN_SCORE: f64 = 0.72;

/// Memory is opt-in: enabled only when `AGENT_PLUGIN_MEMORY=on`.
pub fn enabled() -> bool {
    std::env::var(MEMORY_ENV)
        .map(|s| s.trim().eq_ignore_ascii_case("on"))
        .unwrap_or(false)
}

fn collection() -> String {
    std::env::var(COLLECTION_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_COLLECTION.to_string())
}

/// Extract the JSON array of fact strings from a raw model reply — lenient:
/// fenced blocks and surrounding prose tolerated, non-string entries dropped,
/// per-fact size caps applied.
#[allow(clippy::sliced_string_as_bytes)]
pub fn parse_facts(content: &str) -> Vec<String> {
    let text = llm::strip_tool_markup(content);
    let start = match text.find('[') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let end = match text.rfind(']') {
        Some(e) if e > start => e + 1,
        _ => return Vec::new(),
    };
    serde_json::from_slice::<Vec<Value>>(text[start..end].as_bytes())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .map(|f| f.chars().take(MAX_FACT_CHARS).collect::<String>())
        .take(MAX_FACTS_PER_GOAL)
        .collect()
}

/// Format query hits into the injected context block; `None` when nothing
/// clears the score floor (keeps noise out of the prompt).
pub fn format_facts(hits: &[Value]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for hit in hits {
        let Some(score) = hit.get("score").and_then(Value::as_f64) else { continue };
        if score < MIN_SCORE {
            continue;
        }
        let Some(text) = hit.get("metadata").and_then(|m| m.get("fact")).and_then(Value::as_str)
        else {
            continue;
        };
        lines.push(format!("- ({score:.2}) {text}"));
        if lines.len() >= RECALL_TOP_K as usize {
            break;
        }
    }
    (!lines.is_empty()).then(|| {
        format!(
            "[KNOWN CONTEXT]\nFacts remembered from earlier goals:\n{}",
            lines.join("\n")
        )
    })
}

async fn read_index(rpc: &Rpc, db_timeout_ms: u32) -> Result<Vec<Value>, String> {
    let v = rpc.call("db_get", json!({"key": INDEX_KEY}), db_timeout_ms).await?;
    if v.get("found").and_then(Value::as_bool) != Some(true) {
        return Ok(Vec::new());
    }
    Ok(v.get("value")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()))
        .as_array()
        .cloned()
        .unwrap_or_default())
}

async fn write_index(rpc: &Rpc, db_timeout_ms: u32, ids: &[Value]) -> Result<(), String> {
    let v = rpc.call(
        "db_set",
        json!({"key": INDEX_KEY, "value": ids}),
        db_timeout_ms,
    )
    .await?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!("database.db_set returned unexpected payload: {v}"));
    }
    Ok(())
}

/// Recall facts relevant to `goal_text`. Best-effort by design: any failure
/// logs loudly and yields no block — a broken vector-db must never stop a
/// goal from starting.
pub async fn recall(rpc: &Rpc, goal_text: &str) -> Option<String> {
    let result = rpc
        .call(
            "vec_query",
            json!({
                "collection": collection(),
                "text": goal_text,
                "top_k": RECALL_TOP_K,
            }),
            15_000,
        )
        .await;
    match result {
        Ok(v) => {
            let empty = Vec::new();
            let hits = v.get("results").and_then(Value::as_array).unwrap_or(&empty);
            format_facts(hits)
        }
        Err(e) => {
            eprintln!("[agent] memory recall skipped: {e}");
            None
        }
    }
}

/// Extract durable facts from a completed goal and store them. Fire-and-forget
/// from the caller's perspective: failures log, never surface.
pub async fn remember(rpc: &Rpc, plan: &LlmPlan, goal_id: &str, goal: &str, final_answer: &str) {
    let extraction_prompt = format!(
        "Extract durable, reusable facts from this finished task as a JSON \
         array of short standalone strings (no timestamps, no one-off task \
         state). Return ONLY the JSON array.\n\
         Task: {goal}\nOutcome: {final_answer}"
    );
    let transcript = [store::Turn {
        role: "user".to_string(),
        content: extraction_prompt,
    }];
    let outcome = match llm::chat_with_fallback(rpc, plan, &transcript, &[]).await {
        Ok(o) => o.content,
        Err(e) => {
            eprintln!("[agent] memory extraction failed for goal {goal_id}: {e}");
            return;
        }
    };
    let facts = parse_facts(&outcome);
    if facts.is_empty() {
        return;
    }

    let db_timeout_ms = 5_000;
    let now = store::now_ms();
    let mut index = match read_index(rpc, db_timeout_ms).await {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("[agent] memory index read failed: {e}");
            return;
        }
    };
    for (i, fact) in facts.iter().enumerate() {
        let id = format!("f{now}-{i}");
        let upsert = rpc
            .call(
                "vec_upsert",
                json!({
                    "collection": collection(),
                    "id": id,
                    "text": fact,
                    "metadata": {"fact": fact, "goal_id": goal_id, "ts": now},
                }),
                30_000,
            )
            .await;
        match upsert {
            Ok(_) => index.push(json!({"id": id, "ts": now})),
            Err(e) => eprintln!("[agent] memory store failed for {id}: {e}"),
        }
    }
    if let Err(e) = write_index(rpc, db_timeout_ms, &index).await {
        eprintln!("[agent] memory index write failed: {e}");
    }
    println!("[agent] remembered {} fact(s) from goal {goal_id}", facts.len());
}

/// Delete one stored fact by semantic similarity to `query` (top hit above
/// the score floor). Returns the deleted id when found.
pub async fn forget(rpc: &Rpc, query: &str) -> Result<Option<String>, String> {
    let v = rpc
        .call(
            "vec_query",
            json!({"collection": collection(), "text": query, "top_k": 1}),
            15_000,
        )
        .await?;
    let hit = v
        .get("results")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    if hit.is_null() {
        return Ok(None);
    }
    let score = hit.get("score").and_then(Value::as_f64).unwrap_or(0.0);
    if score < MIN_SCORE {
        return Ok(None);
    }
    let id = hit
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "malformed vec_query result".to_string())?
        .to_string();
    let del = rpc
        .call(
            "vec_delete",
            json!({"collection": collection(), "id": id}),
            10_000,
        )
        .await?;
    if del.get("deleted").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    let db_timeout_ms = 5_000;
    let mut index = read_index(rpc, db_timeout_ms).await?;
    index.retain(|e| e.get("id").and_then(Value::as_str) != Some(&id));
    write_index(rpc, db_timeout_ms, &index).await?;
    Ok(Some(id))
}

/// Wipe every stored fact (via the id index) plus the index itself.
pub async fn clear(rpc: &Rpc) -> Result<usize, String> {
    let db_timeout_ms = 5_000;
    let index = read_index(rpc, db_timeout_ms).await?;
    let mut deleted = 0usize;
    for entry in &index {
        let Some(id) = entry.get("id").and_then(Value::as_str) else { continue };
        let ok = rpc
            .call(
                "vec_delete",
                json!({"collection": collection(), "id": id}),
                10_000,
            )
            .await?
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        deleted += ok as usize;
    }
    write_index(rpc, db_timeout_ms, &[]).await?;
    Ok(deleted)
}

/// Contents of the memory index for `memory_list` / operator inspection.
pub async fn list(rpc: &Rpc) -> Result<Vec<Value>, String> {
    let mut entries = read_index(rpc, 5_000).await?;
    // Newest first — same ordering contract as goal_list.
    entries.sort_by_key(|e| -e.get("ts").and_then(Value::as_i64).unwrap_or(0));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_and_fenced_arrays() {
        assert_eq!(parse_facts("[\"a\", \"b\"]"), vec!["a", "b"]);
        let fenced = "```json\n[\"user runs Arch Linux\"]\n```";
        assert_eq!(parse_facts(fenced), vec!["user runs Arch Linux"]);
        let prose = "Sure! Here are the facts:\n[\"server is at 10.0.0.2\"] hope that helps";
        assert_eq!(parse_facts(prose), vec!["server is at 10.0.0.2"]);
        assert!(parse_facts("no arrays here").is_empty());
        assert!(parse_facts("[\"a\", 42, null]").len() == 1, "non-strings dropped");
    }

    #[test]
    fn caps_fact_count_and_length() {
        let many: Vec<String> = (0..20).map(|i| format!("fact{i}")).collect();
        let raw = serde_json::to_string(&many).unwrap();
        assert_eq!(parse_facts(&raw).len(), MAX_FACTS_PER_GOAL);
        let long = vec!["ж".repeat(MAX_FACT_CHARS + 100)];
        let parsed = parse_facts(&serde_json::to_string(&long).unwrap());
        assert_eq!(parsed[0].chars().count(), MAX_FACT_CHARS);
    }

    #[test]
    fn formats_only_high_scoring_hits() {
        let hits = vec![
            json!({"id": "f1", "score": 0.91, "metadata": {"fact": "deploys via ssh to box7"}}),
            json!({"id": "f2", "score": 0.50, "metadata": {"fact": "noise"}}),
            json!({"id": "f3", "score": 0.80, "metadata": {"fact": "uses ntfy topic vyn"}}),
            json!({"id": "f4", "score": 0.99, "metadata": {}}),
        ];
        let block = format_facts(&hits).unwrap();
        assert!(block.starts_with("[KNOWN CONTEXT]"));
        assert!(block.contains("box7"));
        assert!(block.contains("ntfy"));
        assert!(!block.contains("noise"));
        assert!(!block.contains("(0.99)"), "hit without metadata.fact skipped");
        assert!(format_facts(&[]).is_none());
        assert!(format_facts(&[json!({"id": "x", "score": 0.99, "metadata": {"fact": "y"}})])
            .is_some());
    }
}
