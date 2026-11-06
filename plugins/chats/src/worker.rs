//! Background worker: summaries, embeddings and the delete cascade. One pass
//! at a time, woken by a timer tick and by [`Worker::kick`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use crate::config::Config;
use crate::db::Db;
use crate::rpc::{is_missing_plugin, Rpc};

pub struct Worker {
    pub(crate) db: Db,
    pub(crate) rpc: Rpc,
    pub(crate) cfg: Arc<Config>,
    kick: Notify,
    /// Called after a pass changed synced state (emits the `changed` event).
    pub(crate) changed: Arc<dyn Fn() + Send + Sync>,
    warned: Mutex<HashSet<&'static str>>,
    pass_lock: tokio::sync::Mutex<()>,
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
            kick: Notify::new(),
            changed,
            warned: Mutex::new(HashSet::new()),
            pass_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Wake the worker now (after `append`/`delete`).
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Start the loop. `CHATS_SEMANTIC=off` starts nothing.
    pub fn spawn(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if !self.cfg.semantic {
            return None;
        }
        let me = Arc::clone(self);
        Some(tokio::spawn(async move {
            let tick = Duration::from_secs(me.cfg.work_interval_secs.max(1));
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(tick) => {}
                    _ = me.kick.notified() => {}
                }
                me.run_once(crate::now_ms()).await;
            }
        }))
    }

    /// One full pass. Passes never overlap.
    pub async fn run_once(&self, _now: i64) {
        if !self.cfg.semantic {
            return;
        }
        let _g = self.pass_lock.lock().await;
    }

    /// Log a missing/failed dependency once per kind.
    pub(crate) fn warn_once(&self, key: &'static str, err: &str) {
        let first = self.warned.lock().unwrap().insert(key);
        if first {
            if is_missing_plugin(err) {
                eprintln!("[chats] {key}: plugin not available, skipping ({err})");
            } else {
                eprintln!("[chats] {key}: {err}");
            }
        }
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
        f.worker.run_once(super::super::testkit::T0 + 10_000_000).await;
        assert_eq!(f.fake.call_count(), 0);
    }
}
