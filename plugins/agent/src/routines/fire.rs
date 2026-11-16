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
    let book = store::load(rpc, timeout).await;

    if is_correction(&normalized) {
        if book.correction_target(now).is_some() {
            update_logged(rpc, timeout, |book| match book.correction_target(now) {
                Some(i) => {
                    eprintln!("[agent] routines: correction disables {}", book.routines[i].id);
                    book.disable(i, "user_correction", now);
                    book.last_fire = None;
                    true
                }
                None => false,
            })
            .await;
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
        update_logged(rpc, timeout, |book| disable_by_id(book, &routine.id, "tool_changed", now)).await;
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
        update_logged(rpc, timeout, |book| {
            if let Some(r) = book.routines.iter_mut().find(|r| r.id == routine.id) {
                r.fails += 1;
            }
            disable_by_id(book, &routine.id, "tool_error", now);
            true
        })
        .await;
        return Ok(false);
    }

    // Counters and the reply rotation go to the freshest book; a routine
    // deleted meanwhile still answers from the copy that fired.
    let mut reply = store::update(rpc, timeout, |book| {
        let Some(j) = book.routines.iter().position(|r| r.id == routine.id) else {
            return (false, String::new());
        };
        let reply = book.next_reply(j);
        let r = &mut book.routines[j];
        r.hits += 1;
        r.updated_at_ms = now;
        book.last_fire = Some(LastFire { routine_id: routine.id.clone(), at_ms: now });
        (true, reply)
    })
    .await
    .unwrap_or_else(|e| {
        eprintln!("[agent] routines: save failed: {e}");
        String::new()
    });
    if reply.is_empty() {
        let mut copy = book.clone();
        reply = copy.next_reply(i);
    }
    if reply.is_empty() {
        reply = FALLBACK_REPLY.to_string();
    }

    doc.transcript.push(Turn { role: "assistant".into(), content: reply.clone() });
    doc.status = STATUS_COMPLETED.to_string();
    doc.final_answer = reply;
    engine::next_step(doc, "final", json!({"fast_path": "routine"}));
    engine::persist(db, doc).await?;
    Ok(true)
}

fn disable_by_id(book: &mut super::Book, id: &str, reason: &str, now: i64) -> bool {
    match book.routines.iter().position(|r| r.id == id) {
        Some(j) => {
            book.disable(j, reason, now);
            true
        }
        None => false,
    }
}

async fn update_logged(rpc: &Rpc, timeout: u32, f: impl FnOnce(&mut super::Book) -> bool) {
    if let Err(e) = store::update(rpc, timeout, |book| (f(book), ())).await {
        eprintln!("[agent] routines: save failed: {e}");
    }
}
