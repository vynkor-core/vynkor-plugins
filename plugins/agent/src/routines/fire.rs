//! Goal-start hook: serve a fresh goal from a learned routine, or blame the
//! last fired routine when the goal is a correction.

use serde_json::json;

use super::{allowed_tools, eligible, is_correction, spec_hash, store, LastFire};
use crate::engine;
use crate::smalltalk::normalize;
use crate::store::{now_ms, Db, GoalDoc, Turn, STATUS_COMPLETED};
use crate::tools::Catalog;
use crate::Rpc;

/// Used only if a routine somehow carries no replies (mining never stores one).
const FALLBACK_REPLY: &str = "Готово.";

/// `Ok(true)`: the routine completed the goal. `Ok(false)`: run the normal
/// path. After a failed dispatch `doc.transcript` holds the goal turn and
/// the error observation, which the engine carries into the LLM loop.
pub async fn try_fire(db: &Db, rpc: &Rpc, catalog: &Catalog, doc: &mut GoalDoc) -> Result<bool, String> {
    let timeout = db.timeout_ms();
    let now = now_ms();
    let normalized = normalize(&doc.goal);
    let mut book = store::load(rpc, timeout).await;

    if is_correction(&normalized) {
        if let Some(i) = book.correction_target(now) {
            eprintln!("[agent] routines: correction disables {}", book.routines[i].id);
            book.disable(i, "user_correction", now);
            book.last_fire = None;
            save_logged(rpc, timeout, &book).await;
        }
        return Ok(false);
    }

    let Some(i) = book.find_active(&normalized) else {
        return Ok(false);
    };
    let routine = book.routines[i].clone();
    let allow = allowed_tools();
    let spec = catalog
        .get(&routine.tool)
        .filter(|s| eligible(s, &allow) && spec_hash(s) == routine.spec_hash);
    let Some(spec) = spec else {
        eprintln!("[agent] routines: {} no longer matches its tool; disabled", routine.id);
        book.disable(i, "tool_changed", now);
        save_logged(rpc, timeout, &book).await;
        return Ok(false);
    };
    if engine::check_quota(doc, spec).is_err() {
        return Ok(false);
    }

    doc.transcript = vec![Turn { role: "user".into(), content: doc.goal.clone() }];
    doc.tool_counts.entry(routine.tool.clone()).and_modify(|c| *c += 1).or_insert(1);
    doc.tool_last_ms.insert(routine.tool.clone(), now);
    let ok =
        engine::dispatch_and_observe(rpc, doc, spec.timeout_ms, &routine.tool, routine.params.clone())
            .await?;
    if let Some(detail) = doc.steps.last_mut().and_then(|s| s.detail.as_object_mut()) {
        detail.insert("routine".into(), json!(routine.id));
    }

    let now = now_ms();
    if !ok {
        let r = &mut book.routines[i];
        r.fails += 1;
        book.disable(i, "tool_error", now);
        save_logged(rpc, timeout, &book).await;
        return Ok(false);
    }

    let mut reply = book.next_reply(i);
    if reply.is_empty() {
        reply = FALLBACK_REPLY.to_string();
    }
    let r = &mut book.routines[i];
    r.hits += 1;
    r.updated_at_ms = now;
    book.last_fire = Some(LastFire { routine_id: routine.id.clone(), at_ms: now });

    doc.transcript.push(Turn { role: "assistant".into(), content: reply.clone() });
    doc.status = STATUS_COMPLETED.to_string();
    doc.final_answer = reply;
    engine::next_step(doc, "final", json!({"fast_path": "routine"}));
    engine::persist(db, doc).await?;
    save_logged(rpc, timeout, &book).await;
    Ok(true)
}

async fn save_logged(rpc: &Rpc, timeout: u32, book: &super::Book) {
    if let Err(e) = store::save(rpc, timeout, book).await {
        eprintln!("[agent] routines: save failed: {e}");
    }
}
