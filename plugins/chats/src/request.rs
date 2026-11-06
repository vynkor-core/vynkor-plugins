use rusqlite::Connection;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::{retention, search, store, sync};

pub const ACTIONS: [&str; 11] = [
    "chat_session_resolve",
    "chat_turn_append",
    "chat_sessions_list",
    "chat_session_read",
    "chat_session_update",
    "chat_delete",
    "chat_search",
    "chat_sync",
    "chat_recall",
    "chat_review_get",
    "chat_review_apply",
];

/// Whether a successful `action` with this `result` changed stored state and
/// should publish a `changed` event. Resolve only counts when it created a
/// session; a delete only when it actually deleted.
pub fn is_mutation(action: &str, result: &Value) -> bool {
    match action {
        "chat_session_resolve" => result["created"] == true,
        "chat_delete" => result["deleted"] == true,
        "chat_turn_append" | "chat_session_update" => true,
        "chat_review_apply" => {
            result["deleted"].as_i64().unwrap_or(0) + result["compressed"].as_i64().unwrap_or(0) > 0
        }
        _ => false,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadParams {
    id: String,
    #[serde(default)]
    last: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateParams {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    pinned: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewApplyParams {
    id: String,
    accept: Vec<String>,
    #[serde(default)]
    exclude: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdParams {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncParams {
    since_rev: i64,
    #[serde(default)]
    limit: Option<i64>,
}

pub(crate) fn parse<T: DeserializeOwned>(action: &str, raw: &[u8]) -> Result<T, String> {
    let raw: &[u8] = if raw.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        raw
    };
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
        "chat_review_get" => {
            let _: NoParams = parse(action, params)?;
            retention::review_get(conn)
        }
        "chat_review_apply" => {
            let p: ReviewApplyParams = parse(action, params)?;
            retention::review_apply(
                conn,
                cfg,
                &p.id,
                &p.accept,
                p.exclude.as_deref().unwrap_or(&[]),
                now,
            )
        }
        "chat_recall" => Err("internal error: chat_recall is served by the async service".into()),
        other => Err(format!("unknown action {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::schema::open_temp;

    #[test]
    fn unknown_param_fields_are_rejected() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let cases = [
            ("chat_turn_append", r#"{"sessionId":"x","status":"ok"}"#),
            (
                "chat_session_resolve",
                r#"{"device":"d","kind":"chat","sesion_id":"x"}"#,
            ),
            ("chat_sessions_list", r#"{"limt":3}"#),
            ("chat_session_read", r#"{"id":"x","lst":3}"#),
            ("chat_session_update", r#"{"id":"x","titel":"t"}"#),
            ("chat_delete", r#"{"id":"x","force":true}"#),
            ("chat_search", r#"{"query":"a","limt":3}"#),
            ("chat_sync", r#"{"since_rev":0,"sinceRev":1}"#),
        ];
        for (action, params) in cases {
            let e = handle(&mut c, &cfg, action, params.as_bytes(), 1).unwrap_err();
            assert!(
                e.contains("invalid params") && e.contains("unknown field"),
                "{action}: {e}"
            );
        }
    }
}
