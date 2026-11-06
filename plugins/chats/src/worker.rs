//! Background worker. Two independent lanes so a hung `ai` can never delay
//! deletions: the *work* lane (summaries, embeddings) and the *cleanup* lane
//! (delete cascade, daily sweep). Each runs one pass at a time, woken by a
//! timer tick and by [`Worker::kick`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use crate::config::Config;
use crate::db::Db;
use crate::rpc::{is_missing_plugin, Rpc};

/// Wall-clock budget of one cascade/sweep pass.
const PASS_BUDGET_MS: u64 = 60_000;
/// A repeating failure is logged again every this many occurrences.
const WARN_EVERY: u64 = 30;

pub struct Worker {
    pub(crate) db: Db,
    pub(crate) rpc: Rpc,
    pub(crate) cfg: Arc<Config>,
    kick_work: Notify,
    kick_cleanup: Notify,
    /// Called after a pass changed synced state (emits the `changed` event).
    pub(crate) changed: Arc<dyn Fn() + Send + Sync>,
    failures: Mutex<HashMap<&'static str, u64>>,
    /// Sessions whose summary call failed at transport level: retry-after (ms).
    pub(crate) summary_retry_at: Mutex<HashMap<String, i64>>,
    work_lock: tokio::sync::Mutex<()>,
    cleanup_lock: tokio::sync::Mutex<()>,
    budget_ms: AtomicU64,
}

impl Worker {
    pub fn new(
        db: Db,
        rpc: Rpc,
        cfg: Arc<Config>,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Worker {
            db,
            rpc,
            cfg,
            kick_work: Notify::new(),
            kick_cleanup: Notify::new(),
            changed,
            failures: Mutex::new(HashMap::new()),
            summary_retry_at: Mutex::new(HashMap::new()),
            work_lock: tokio::sync::Mutex::new(()),
            cleanup_lock: tokio::sync::Mutex::new(()),
            budget_ms: AtomicU64::new(PASS_BUDGET_MS),
        })
    }

    /// Wake both lanes now (after `append`/`delete`).
    pub fn kick(&self) {
        self.kick_work.notify_one();
        self.kick_cleanup.notify_one();
    }

    pub(crate) fn kick_cleanup(&self) {
        self.kick_cleanup.notify_one();
    }

    pub(crate) fn pass_budget(&self) -> Duration {
        Duration::from_millis(self.budget_ms.load(Ordering::Relaxed))
    }

    #[cfg(test)]
    pub(crate) fn set_pass_budget_ms(&self, ms: u64) {
        self.budget_ms.store(ms, Ordering::Relaxed);
    }

    /// Start both lanes. `CHATS_SEMANTIC=off` starts nothing.
    pub fn spawn(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if !self.cfg.semantic {
            return None;
        }
        let (a, b) = (Arc::clone(self), Arc::clone(self));
        Some(tokio::spawn(async move {
            let tick = Duration::from_secs(a.cfg.work_interval_secs.max(1));
            let work = async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(tick) => {}
                        _ = a.kick_work.notified() => {}
                    }
                    a.run_work(crate::now_ms()).await;
                }
            };
            let cleanup = async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(tick) => {}
                        _ = b.kick_cleanup.notified() => {}
                    }
                    b.run_cleanup(crate::now_ms()).await;
                }
            };
            tokio::join!(work, cleanup);
        }))
    }

    /// Work lane: summaries, then embeddings.
    pub async fn run_work(&self, now: i64) {
        if !self.cfg.semantic {
            return;
        }
        let _g = self.work_lock.lock().await;
        if self.summarize_pass(now).await {
            (self.changed)();
        }
        self.embed_pass().await;
    }

    /// Cleanup lane: delete cascade, then the daily sweep.
    pub async fn run_cleanup(&self, now: i64) {
        if !self.cfg.semantic {
            return;
        }
        let _g = self.cleanup_lock.lock().await;
        if self.cascade_pass(now).await {
            (self.changed)();
        }
        self.sweep_pass(now).await;
    }

    /// Both lanes, one after the other (deterministic; used by tests).
    pub async fn run_once(&self, now: i64) {
        self.run_work(now).await;
        self.run_cleanup(now).await;
    }

    /// Log a dependency failure: the first time, then every [`WARN_EVERY`]th
    /// repeat, and again after [`Worker::ok`] reported recovery.
    pub(crate) fn warn(&self, key: &'static str, err: &str) -> bool {
        let n = {
            let mut m = self.failures.lock().unwrap();
            let n = m.entry(key).or_insert(0);
            *n += 1;
            *n
        };
        let log = n == 1 || n % WARN_EVERY == 0;
        if log {
            let again = if n > 1 {
                format!(" (failed {n} times)")
            } else {
                String::new()
            };
            if is_missing_plugin(err) {
                eprintln!("[chats] {key}: plugin not available ({err}){again}");
            } else {
                eprintln!("[chats] {key}: {err}{again}");
            }
        }
        log
    }

    /// A call to `key` worked: the next failure is logged afresh.
    pub(crate) fn ok(&self, key: &'static str) {
        self.failures.lock().unwrap().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use crate::testkit::Fixture;

    #[tokio::test]
    async fn semantic_off_makes_no_outbound_calls() {
        let f = Fixture::with_config(|c| c.semantic = false);
        let sid = f.session("lap", "chat", super::super::testkit::T0);
        f.turn(&sid, "weather", "sunny", super::super::testkit::T0);
        f.delete(&sid).await;
        assert!(f.worker.spawn().is_none());
        f.worker.kick();
        f.worker
            .run_once(super::super::testkit::T0 + 10_000_000)
            .await;
        assert_eq!(f.fake.call_count(), 0);
    }

    #[tokio::test]
    async fn warnings_repeat_on_recovery_and_every_nth_failure() {
        let f = Fixture::new();
        let w = &f.worker;
        assert!(w.warn("agent", "boom"), "first failure is logged");
        assert!(!w.warn("agent", "boom"), "repeats are quiet");
        w.ok("agent");
        assert!(w.warn("agent", "boom"), "down -> up -> down logs again");
        let logged = (0..70).filter(|_| w.warn("agent", "boom")).count();
        assert_eq!(logged, 2, "a stuck dependency is logged every 30th failure");
        assert!(w.warn("vector-db", "other"), "keys are independent");
    }
}
