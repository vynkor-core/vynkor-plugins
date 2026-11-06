//! Delete cascade (queue side). The worker half is added with the cascade pass.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::json;

fn db_err(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

/// (Re)queue cleanup of vectors for `session_id`: used when the worker finds
/// that a session died while it was writing vectors. Merges with a pending
/// job and clears its `done` list so every step runs again.
pub fn requeue(conn: &mut Connection, session_id: &str, turn_ids: &[String]) -> Result<(), String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT turn_ids FROM cascade WHERE session_id = ?",
            [session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let mut ids: Vec<String> = existing
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for t in turn_ids {
        if !ids.contains(t) {
            ids.push(t.clone());
        }
    }
    tx.execute(
        "INSERT INTO cascade(session_id, turn_ids, goal_ids, attempts, next_at, done)
         VALUES (?1, ?2, '[]', 0, 0, '[]')
         ON CONFLICT(session_id) DO UPDATE SET turn_ids = ?2, attempts = 0, next_at = 0, done = '[]'",
        params![session_id, json!(ids).to_string()],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)
}
