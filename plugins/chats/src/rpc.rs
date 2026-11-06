//! Outbound kernel-routed actions (`ai`, `vector-db`, `agent`).
//!
//! Same scheme as the `agent` plugin: a task hands an [`RpcCall`] to the
//! serve loop through a channel; the loop sends it as an `ActionRequest`
//! with id `rpc-{seq}` and correlates the `ActionResponse` by that id.

use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// One pending kernel-routed action handed to the serve loop.
pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self { tx }
    }

    /// One kernel-routed action round-trip. Resolves to the decoded
    /// `data_json` on `ACTION_OK`. Failures are `"{action} failed: {error}"`
    /// (kernel or target error) or `"{action} timed out after {ms} ms"`.
    pub async fn call(
        &self,
        action: &str,
        params: Value,
        timeout_ms: u32,
    ) -> Result<Value, String> {
        let params_json = serde_json::to_vec(&params)
            .map_err(|e| format!("failed to encode {action} params: {e}"))?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RpcCall {
                action: action.to_string(),
                params_json,
                timeout_ms,
                reply,
            })
            .await
            .map_err(|_| format!("{action} aborted: serve loop is shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("{action} aborted: serve loop is shutting down")),
            Err(_) => Err(format!("{action} timed out after {effective} ms")),
        }
    }
}

/// The kernel answers `action not found` when the plugin owning an action is
/// not installed or not running.
pub fn is_missing_plugin(err: &str) -> bool {
    err.contains("action not found")
}

/// Serve-loop side of [`Rpc`]: numbers calls (`rpc-{seq}`), remembers who
/// waits for which id and resolves them from `ActionResponse`s.
#[derive(Default)]
pub struct Router {
    seq: u64,
    pending: std::collections::HashMap<String, (String, oneshot::Sender<Result<Value, String>>)>,
}

impl Router {
    /// Register `call` and build the request envelope to send to the kernel.
    pub fn outgoing(&mut self, call: RpcCall) -> vynkor_sdk::proto::Envelope {
        use vynkor_sdk::proto::{envelope, ActionRequest, Envelope};
        self.seq += 1;
        let action_id = format!("rpc-{}", self.seq);
        let RpcCall {
            action,
            params_json,
            timeout_ms,
            reply,
        } = call;
        self.pending
            .insert(action_id.clone(), (action.clone(), reply));
        Envelope {
            payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                action_id,
                action,
                params_json,
                timeout_ms,
                streaming: false,
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    /// Resolve the waiter of `resp`. Unknown ids (late answers after the
    /// caller timed out and was dropped, or foreign ids) are ignored.
    pub fn incoming(&mut self, resp: vynkor_sdk::proto::ActionResponse) {
        use vynkor_sdk::proto::ActionStatus;
        if let Some((action, reply)) = self.pending.remove(&resp.action_id) {
            let result = if resp.status == ActionStatus::ActionOk as i32 {
                serde_json::from_slice::<Value>(&resp.data_json)
                    .map_err(|e| format!("malformed payload: {e}"))
            } else {
                Err(format!("{action} failed: {}", resp.error))
            };
            let _ = reply.send(result);
        }
    }

    /// Drop entries whose waiter has gone (timed out) so they do not pile up.
    pub fn prune(&mut self) {
        self.pending.retain(|_, (_, tx)| !tx.is_closed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::FakeKernel;
    use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus};

    /// Pump `Rpc` calls through a real `Router` into the fake kernel and
    /// answer with proper `ActionResponse` envelopes.
    fn wired(fake: FakeKernel) -> Rpc {
        let (tx, mut rx) = mpsc::channel::<RpcCall>(16);
        tokio::spawn(async move {
            let mut router = Router::default();
            let (done_tx, mut done_rx) = mpsc::channel::<ActionResponse>(16);
            loop {
                tokio::select! {
                    Some(call) = rx.recv() => {
                        let env = router.outgoing(call);
                        let Some(envelope::Payload::ActionRequest(req)) = env.payload else { panic!() };
                        assert!(req.action_id.starts_with("rpc-"));
                        let fake = fake.clone();
                        let done = done_tx.clone();
                        tokio::spawn(async move {
                            let params = serde_json::from_slice(&req.params_json).unwrap();
                            let resp = match fake.handle(&req.action, params).await {
                                Ok(v) => ActionResponse {
                                    action_id: req.action_id,
                                    status: ActionStatus::ActionOk as i32,
                                    data_json: serde_json::to_vec(&v).unwrap(),
                                    error: String::new(),
                                },
                                Err(e) => ActionResponse {
                                    action_id: req.action_id,
                                    status: ActionStatus::ActionError as i32,
                                    data_json: Vec::new(),
                                    error: e,
                                },
                            };
                            let _ = done.send(resp).await;
                        });
                    }
                    Some(resp) = done_rx.recv() => router.incoming(resp),
                    else => break,
                }
            }
        });
        Rpc::new(tx)
    }

    #[tokio::test]
    async fn rpc_roundtrip_through_fake_kernel() {
        let fake = FakeKernel::new();
        let rpc = wired(fake.clone());
        let r = rpc
            .call(
                "vec_upsert_batch",
                serde_json::json!({"collection": "c", "docs": [{"id": "a", "text": "hello", "metadata": {}}]}),
                2000,
            )
            .await
            .unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(fake.doc_ids("c"), vec!["a"]);
        // kernel error is prefixed, missing plugin is recognisable
        fake.set_missing("vector-db", true);
        let e = rpc
            .call("vec_stats", serde_json::json!({"collection": "c"}), 2000)
            .await
            .unwrap_err();
        assert!(e.starts_with("vec_stats failed: "), "{e}");
        assert!(is_missing_plugin(&e));
        assert!(!is_missing_plugin("vec_stats failed: backend unavailable"));
    }

    #[tokio::test]
    async fn rpc_timeout_is_bounded() {
        let fake = FakeKernel::new();
        fake.set_delay("vector-db", 2000);
        let rpc = wired(fake);
        let t = std::time::Instant::now();
        let e = rpc
            .call("vec_stats", serde_json::json!({"collection": "c"}), 100)
            .await
            .unwrap_err();
        assert_eq!(e, "vec_stats timed out after 100 ms");
        assert!(t.elapsed() < Duration::from_millis(1000));
    }
}
