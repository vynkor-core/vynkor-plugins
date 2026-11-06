use rusqlite::Connection;
use serde_json::Value;

use crate::config::Config;

pub const ACTIONS: [&str; 8] = [
    "chat_session_resolve",
    "chat_turn_append",
    "chat_sessions_list",
    "chat_session_read",
    "chat_session_update",
    "chat_delete",
    "chat_search",
    "chat_sync",
];

pub fn is_mutation(action: &str) -> bool {
    matches!(
        action,
        "chat_session_resolve" | "chat_turn_append" | "chat_session_update" | "chat_delete"
    )
}

pub fn handle(
    _conn: &mut Connection,
    _cfg: &Config,
    action: &str,
    _params: &[u8],
    _now: i64,
) -> Result<Value, String> {
    Err(format!("{action}: not implemented"))
}
