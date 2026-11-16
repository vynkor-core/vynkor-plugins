//! The whole routine set as one `database` doc under [`BOOK_KEY`] — it is
//! dozens of entries, read once per fresh goal, so one doc beats a
//! per-routine doc plus an index that can drift.

use serde_json::{json, Value};

use super::Book;
use crate::Rpc;

pub const BOOK_KEY: &str = "routines";

/// Never fails: a missing, unreadable or corrupt doc reads as an empty
/// book (logged), because a routine problem must never stop a goal.
pub async fn load(rpc: &Rpc, timeout_ms: u32) -> Book {
    let v = match rpc.call("db_get", json!({"key": BOOK_KEY}), timeout_ms).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[agent] routines: load failed: {e}");
            return Book::default();
        }
    };
    if v.get("found").and_then(Value::as_bool) != Some(true) {
        return Book::default();
    }
    match serde_json::from_value(v.get("value").cloned().unwrap_or(Value::Null)) {
        Ok(book) => book,
        Err(e) => {
            eprintln!("[agent] routines: stored book is corrupt, ignoring it: {e}");
            Book::default()
        }
    }
}

pub async fn save(rpc: &Rpc, timeout_ms: u32, book: &Book) -> Result<(), String> {
    let v = rpc.call("db_set", json!({"key": BOOK_KEY, "value": book}), timeout_ms).await?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!("database.db_set returned unexpected payload: {v}"));
    }
    Ok(())
}

/// Read-modify-write of the book under one process-wide lock. Goals, mining
/// and `routine_delete` all write the whole doc; without the lock a writer
/// holding an older copy (a fire waiting on its tool) overwrote the others —
/// e.g. re-enabling a routine a correction had just disabled. `f` returns
/// whether it changed anything; only then is the book saved. Keep slow work
/// (tool dispatch, LLM calls) outside `f`.
pub async fn update<T>(
    rpc: &Rpc,
    timeout_ms: u32,
    f: impl FnOnce(&mut Book) -> (bool, T),
) -> Result<T, String> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _held = LOCK.lock().await;
    let mut book = load(rpc, timeout_ms).await;
    let (changed, out) = f(&mut book);
    if changed {
        save(rpc, timeout_ms, &book).await?;
    }
    Ok(out)
}
