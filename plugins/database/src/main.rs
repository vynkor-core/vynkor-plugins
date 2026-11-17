//! `database` plugin — per-caller-namespaced KV + raw SQL storage, gated by
//! `PERMISSION_STORAGE`. See
//! docs/superpowers/specs/2026-07-15-database-plugin-design.md for the design.
//!
//! This is a hot-path plugin (roadmap: "database will be called far more
//! often and needs real concurrency"), so it drives the SDK's concurrent
//! message loop ([`ConcurrentHandler`] + [`serve_concurrent`], implemented
//! in the library crate) instead of the sequential `Plugin::serve`. The SDK
//! loop spawns a handler task per inbound `ActionRequest`, funnels completed
//! replies back through an mpsc channel to the single task that owns the
//! client, isolates handler panics as `ACTION_ERROR` responses, and never
//! shares the client behind a lock — so a replying handler can't deadlock
//! against the loop parked in `recv()`. Replies may come back out of order —
//! the kernel matches on `action_id`.

use std::sync::Arc;

use database_plugin::db::DbConfig;
use database_plugin::handler::Handler;
use vynkor_sdk::concurrent::serve_concurrent;
use vynkor_sdk::{VynkorClient, VynkorError};

fn load_config() -> DbConfig {
    // the kernel grants every plugin its own dir as VYN_DATA_DIR
    let data_dir = std::env::var("DATABASE_PLUGIN_DATA_DIR")
        .or_else(|_| std::env::var("VYN_DATA_DIR"))
        .unwrap_or_else(|_| {
            panic!("DATABASE_PLUGIN_DATA_DIR (or the kernel's VYN_DATA_DIR) must be set")
        });
    let pool_size = std::env::var("DATABASE_PLUGIN_POOL_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let busy_timeout_ms = std::env::var("DATABASE_PLUGIN_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);
    let max_db_bytes = std::env::var("DATABASE_PLUGIN_MAX_DB_BYTES")
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
    let max_value_bytes = std::env::var("DATABASE_PLUGIN_MAX_VALUE_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024 * 1024);
    let max_response_bytes = std::env::var("DATABASE_PLUGIN_MAX_RESPONSE_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4 * 1024 * 1024);

    let handler = Arc::new(Handler::new(
        load_config(),
        max_value_bytes,
        max_response_bytes,
    ));

    let client = VynkorClient::connect_from_env().await?;
    let token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    serve_concurrent(client, &token, handler).await?;

    println!("[database] shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use database_plugin::db::DbConfig;
    use database_plugin::handler::Handler;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use vynkor_sdk::concurrent::run_concurrent_loop;
    use vynkor_sdk::proto::{envelope, ActionRequest, ActionStatus, Envelope, PluginShutdown};
    use vynkor_sdk::VynkorClient;

    /// Regression test for the deadlock this task fixes: drives the
    /// concurrent loop over a real `VynkorClient` (no live kernel needed —
    /// `UnixStream::pair` plus `VynkorClient::from_stream` is the SDK's own
    /// test pattern, see `vynkor-sdk/tests/protocol.rs`).
    ///
    /// The fake "kernel" fires a batch of `ActionRequest`s back-to-back and
    /// then does *not* send anything else until it has read back every
    /// response. Under the old `Arc<Mutex<VynkorClient>>` design this would
    /// deadlock: the loop task's `recv()` holds the lock while waiting for a
    /// frame that (by construction, in this test) never arrives until the
    /// responses are drained — but draining them is exactly what's blocked
    /// on that lock. The `tokio::time::timeout` wrapper turns "would have
    /// hung forever" into a clean test failure instead of an actual CI hang.
    #[tokio::test]
    async fn concurrent_requests_get_responses_without_deadlocking() {
        let dir = tempfile::tempdir().unwrap();
        let handler = Arc::new(Handler::new(
            DbConfig {
                data_dir: dir.path().to_path_buf(),
                pool_size: 4,
                busy_timeout_ms: 2000,
                max_db_bytes: 0,
            },
            1024 * 1024,
            4 * 1024 * 1024,
        ));

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);

        let loop_task = tokio::spawn(run_concurrent_loop(client, handler));

        const N: usize = 20;
        for i in 0..N {
            let req = Envelope {
                payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                    action_id: format!("action-{i}"),
                    action: "db_set".into(),
                    params_json: serde_json::to_vec(&serde_json::json!({
                        "key": format!("k{i}"),
                        "value": i,
                    }))
                    .unwrap(),
                    timeout_ms: 0,
                    streaming: false,
                    caller_plugin_id: "caller_x".into(),
                })),
                ..Default::default()
            };
            kernel.send("database", req).await.unwrap();
        }

        let mut seen = std::collections::HashSet::new();
        let mut changed_events = 0usize;
        // 2N envelopes: every db_set yields its ActionResponse plus one
        // best-effort `changed` event (v0.3 change events).
        for _ in 0..(2 * N) {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for response — loop likely deadlocked")
                .unwrap();
            match env.payload {
                Some(envelope::Payload::ActionResponse(resp)) => {
                    assert_eq!(resp.status, ActionStatus::ActionOk as i32);
                    assert!(seen.insert(resp.action_id), "duplicate response");
                }
                Some(envelope::Payload::EventPublish(ev)) => {
                    assert_eq!(ev.event_type, "changed");
                    changed_events += 1;
                }
                other => panic!("unexpected payload: {other:?}"),
            }
        }
        assert_eq!(seen.len(), N, "expected one response per request");
        assert_eq!(changed_events, N, "expected one change event per db_set");

        // Ask the loop to exit cleanly and make sure it does.
        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                reason: "test done".into(),
                grace_seconds: 0,
            })),
            ..Default::default()
        };
        kernel.send("database", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("run_loop did not exit after PluginShutdown")
            .unwrap()
            .unwrap();
    }

    /// Direct test of the panic-isolation mechanism the SDK loop relies on:
    /// a panic inside a `tokio::spawn`ed task does not unwind the task that
    /// `.await`s its `JoinHandle` — it surfaces as `Err(JoinError)` with
    /// `is_panic() == true`. This is what lets the SDK turn a panicking
    /// handler into an `ACTION_ERROR` response instead of dropping the reply
    /// on the floor.
    #[tokio::test]
    async fn spawned_task_panic_is_observable_as_a_join_error() {
        let join = tokio::spawn(async { panic!("boom") });
        let result: Result<(), _> = join.await;
        let err = result.expect_err("panicking task should yield Err(JoinError)");
        assert!(err.is_panic(), "expected a panic JoinError, got: {err:?}");
    }

    #[tokio::test]
    async fn concurrent_sets_and_gets_across_callers_do_not_cross_talk() {
        let dir = tempfile::tempdir().unwrap();
        let handler = Arc::new(Handler::new(
            DbConfig {
                data_dir: dir.path().to_path_buf(),
                pool_size: 4,
                busy_timeout_ms: 2000,
                max_db_bytes: 0,
            },
            1024 * 1024,
            4 * 1024 * 1024,
        ));

        let mut tasks = Vec::new();
        for caller_n in 0..8 {
            let h = handler.clone();
            tasks.push(tokio::spawn(async move {
                let caller_id = format!("caller_{caller_n}");
                for i in 0..20 {
                    let params = serde_json::to_vec(&serde_json::json!({
                        "key": format!("k{i}"),
                        "value": caller_n,
                    }))
                    .unwrap();
                    h.handle(&caller_id, "db_set", &params).await.unwrap();
                }
                for i in 0..20 {
                    let params =
                        serde_json::to_vec(&serde_json::json!({"key": format!("k{i}")})).unwrap();
                    let out = h.handle(&caller_id, "db_get", &params).await.unwrap();
                    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
                    assert_eq!(v["value"], caller_n, "cross-talk detected for {caller_id}");
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    }
}
