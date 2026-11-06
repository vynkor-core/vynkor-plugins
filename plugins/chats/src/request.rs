use rusqlite::Connection;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::{search, store, sync};

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

#[derive(Deserialize)]
struct ReadParams {
    id: String,
    #[serde(default)]
    last: Option<u32>,
}

#[derive(Deserialize)]
struct UpdateParams {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    pinned: Option<bool>,
}

#[derive(Deserialize)]
struct IdParams {
    id: String,
}

#[derive(Deserialize)]
struct SyncParams {
    since_rev: i64,
    #[serde(default)]
    limit: Option<i64>,
}

fn parse<T: DeserializeOwned>(action: &str, raw: &[u8]) -> Result<T, String> {
    let raw: &[u8] = if raw.iter().all(u8::is_ascii_whitespace) { b"{}" } else { raw };
    serde_json::from_slice(raw).map_err(|e| format!("{action}: invalid params: {e}"))
}

/// Dispatch one action. Runs synchronously under the caller's connection
/// lock; `now` is the host clock in unix ms.
pub fn handle(
    conn: &mut Connection,
    cfg: &Config,
    action: &str,
    params: &[u8],
    now: i64,
) -> Result<Value, String> {
    match action {
        "chat_session_resolve" => store::resolve(conn, cfg, &parse(action, params)?, now),
        "chat_turn_append" => store::append(conn, cfg, parse(action, params)?, now),
        "chat_sessions_list" => store::list(conn, &parse(action, params)?),
        "chat_session_read" => {
            let p: ReadParams = parse(action, params)?;
            store::read(conn, &p.id, p.last)
        }
        "chat_session_update" => {
            let p: UpdateParams = parse(action, params)?;
            store::update(conn, &p.id, p.title, p.pinned)
        }
        "chat_delete" => {
            let p: IdParams = parse(action, params)?;
            store::delete(conn, &p.id)
        }
        "chat_search" => search::search(conn, &parse(action, params)?),
        "chat_sync" => {
            let p: SyncParams = parse(action, params)?;
            let limit = p.limit.unwrap_or(cfg.sync_max as i64).max(1) as usize;
            sync::sync(conn, p.since_rev.max(0), limit, cfg.sync_max)
        }
        other => Err(format!("unknown action {other}")),
    }
}
