//! Goals whose engine is running right now, so `goal_cancel` can stop them.
//!
//! A goal is found by its id or by the caller-chosen `cancel_key` passed to
//! `goal_start` (the caller does not know the goal id until the call returns).
//! Cancelling wakes the goal's [`Notify`]; the engine future is dropped at its
//! next await point and the goal ends as `cancelled`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

struct Entry {
    cancel_key: Option<String>,
    stop: Arc<Notify>,
}

/// Shared registry of running goals; cheap to clone.
#[derive(Clone, Default)]
pub struct Running(Arc<Mutex<HashMap<String, Entry>>>);

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.0.lock().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("Running").field("goals", &n).finish()
    }
}

/// Keeps the goal registered; unregisters on drop.
pub struct Guard {
    running: Running,
    id: String,
    pub stop: Arc<Notify>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.running.0.lock() {
            m.remove(&self.id);
        }
    }
}

impl Running {
    pub fn register(&self, id: &str, cancel_key: Option<String>) -> Guard {
        let stop = Arc::new(Notify::new());
        if let Ok(mut m) = self.0.lock() {
            m.insert(
                id.to_string(),
                Entry {
                    cancel_key,
                    stop: Arc::clone(&stop),
                },
            );
        }
        Guard {
            running: self.clone(),
            id: id.to_string(),
            stop,
        }
    }

    /// Wake every running goal matching `id` or `cancel_key`; returns their ids.
    pub fn cancel(&self, id: Option<&str>, cancel_key: Option<&str>) -> Vec<String> {
        let Ok(m) = self.0.lock() else {
            return Vec::new();
        };
        let mut ids: Vec<String> = m
            .iter()
            .filter(|(gid, e)| {
                id.is_some_and(|i| i == gid.as_str())
                    || cancel_key.is_some_and(|k| e.cancel_key.as_deref() == Some(k))
            })
            .map(|(gid, e)| {
                // notify_one stores a permit: a goal that has not reached its
                // select yet still stops as soon as it does.
                e.stop.notify_one();
                gid.clone()
            })
            .collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_by_key_wakes_and_drop_unregisters() {
        let r = Running::default();
        let g = r.register("7", Some("k1".into()));
        assert_eq!(r.cancel(None, Some("k1")), vec!["7".to_string()]);
        // permit stored: resolves immediately
        tokio::time::timeout(std::time::Duration::from_millis(100), g.stop.notified())
            .await
            .unwrap();
        drop(g);
        assert!(r.cancel(Some("7"), None).is_empty());
    }

    #[test]
    fn unknown_key_or_id_cancels_nothing() {
        let r = Running::default();
        let _g = r.register("1", None);
        assert!(r.cancel(None, Some("nope")).is_empty());
        assert!(r.cancel(Some("2"), None).is_empty());
        assert!(r.cancel(None, None).is_empty());
    }
}
