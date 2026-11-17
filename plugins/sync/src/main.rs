//! `sync` plugin — host-side versioned KV state store (SQLite-backed),
//! publishing a `sync.delta` event on every mutation. See
//! `docs/REMOTE_DEVICES_PLAN.md` §11 and the D-13 task in
//! `docs/REMOTE_DEVICES_ROADMAP.md` (kernel repo) for the design.
//!
//! Hot-path plugin (like `database`), so it drives the SDK's concurrent
//! message loop ([`ConcurrentHandler`] + [`serve_concurrent`], implemented
//! in the library crate) instead of the sequential `Plugin::serve`. One task
//! owns the client and `tokio::select!`s between inbound frames and a
//! channel of completed replies; handlers run in spawned tasks and reply out
//! of order (the kernel matches on `action_id`).

use std::sync::Arc;

use sync_plugin::db::DbConfig;
use sync_plugin::handler::SyncHandler;
use vynkor_sdk::concurrent::serve_concurrent;
use vynkor_sdk::{VynkorClient, VynkorError};

fn load_config() -> DbConfig {
    // the kernel grants every plugin its own dir as VYN_DATA_DIR
    let data_dir = std::env::var("SYNC_PLUGIN_DATA_DIR")
        .or_else(|_| std::env::var("VYN_DATA_DIR"))
        .unwrap_or_else(|_| {
            panic!("SYNC_PLUGIN_DATA_DIR (or the kernel's VYN_DATA_DIR) must be set")
        });
    let pool_size = std::env::var("SYNC_PLUGIN_POOL_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let busy_timeout_ms = std::env::var("SYNC_PLUGIN_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);
    let max_db_bytes = std::env::var("SYNC_PLUGIN_MAX_DB_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256 * 1024 * 1024);
    DbConfig {
        data_dir: data_dir.into(),
        pool_size,
        busy_timeout_ms,
        max_db_bytes,
    }
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let max_value_bytes = std::env::var("SYNC_PLUGIN_MAX_VALUE_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024 * 1024);
    let max_response_bytes = std::env::var("SYNC_PLUGIN_MAX_RESPONSE_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4 * 1024 * 1024);
    let heartbeat_ttl_secs = std::env::var("SYNC_PLUGIN_HEARTBEAT_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);

    let handler = Arc::new(
        SyncHandler::open(
            load_config(),
            max_value_bytes,
            max_response_bytes,
            heartbeat_ttl_secs,
        )
        .await
        .unwrap_or_else(|e| panic!("failed to open sync database: {e}")),
    );

    let client = VynkorClient::connect_from_env().await?;
    let token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    serve_concurrent(client, &token, handler).await?;

    println!("[sync] shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    use sync_plugin::db::DbConfig;
    use sync_plugin::handler::SyncHandler;
    use tokio::net::UnixStream;
    use vynkor_sdk::concurrent::run_concurrent_loop;
    use vynkor_sdk::proto::{envelope, ActionRequest, ActionStatus, Envelope, PluginShutdown};
    use vynkor_sdk::VynkorClient;

    /// Drives the concurrent loop over a real `VynkorClient` (no live kernel
    /// needed — `UnixStream::pair` plus `VynkorClient::from_stream` is the
    /// SDK's own test pattern). The fake "kernel" fires a `sync_set` and
    /// asserts that the `ActionResponse` comes back first and the
    /// `sync.delta` `EventPublish` envelope arrives right after it, with the
    /// expected delta payload.
    #[tokio::test]
    async fn sync_set_response_then_delta_event_without_deadlocking() {
        let dir = tempfile::tempdir().unwrap();
        let handler = Arc::new(
            SyncHandler::open(
                DbConfig {
                    data_dir: dir.path().to_path_buf(),
                    pool_size: 4,
                    busy_timeout_ms: 2000,
                    max_db_bytes: 0,
                },
                1024 * 1024,
                4 * 1024 * 1024,
                300,
            )
            .await
            .unwrap(),
        );

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);

        let loop_task = tokio::spawn(run_concurrent_loop(client, handler));

        let req = Envelope {
            payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                action_id: "action-1".into(),
                action: "sync_set".into(),
                params_json: serde_json::to_vec(&serde_json::json!({
                    "key": "foo",
                    "value": {"n": 1},
                }))
                .unwrap(),
                timeout_ms: 0,
                streaming: false,
                caller_plugin_id: "caller_x".into(),
            })),
            ..Default::default()
        };
        kernel.send("sync", req).await.unwrap();

        // First envelope is the ActionResponse.
        let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
            .await
            .expect("timed out waiting for response — loop likely deadlocked")
            .unwrap();
        match env.payload {
            Some(envelope::Payload::ActionResponse(resp)) => {
                assert_eq!(resp.status, ActionStatus::ActionOk as i32);
                assert_eq!(resp.action_id, "action-1");
            }
            other => panic!("expected ActionResponse, got {other:?}"),
        }

        // Second envelope is the EventPublish carrying the sync.delta.
        let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
            .await
            .expect("timed out waiting for delta event")
            .unwrap();
        match env.payload {
            Some(envelope::Payload::EventPublish(ev)) => {
                assert_eq!(ev.event_type, "sync.delta");
                let payload: serde_json::Value = serde_json::from_slice(&ev.payload_json).unwrap();
                assert_eq!(payload["op"], "set");
                assert_eq!(payload["key"], "foo");
                assert_eq!(payload["value"], serde_json::json!({"n": 1}));
                assert_eq!(payload["version"], 1);
                assert!(payload["updated_at"].as_i64().is_some());
            }
            other => panic!("expected EventPublish, got {other:?}"),
        }

        // Ask the loop to exit cleanly and make sure it does.
        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                reason: "test done".into(),
                grace_seconds: 0,
            })),
            ..Default::default()
        };
        kernel.send("sync", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("run_loop did not exit after PluginShutdown")
            .unwrap()
            .unwrap();
    }
}
