//! Lets concurrently spawned handler tasks make outbound action calls (e.g.
//! `chat_completion` -> `network`'s `http_request`) without touching the
//! single [`VynkorClient`] connection directly.
//!
//! `main.rs`'s doc comment explains why a second connection isn't an option:
//! the kernel refuses a second registration under the same `plugin_id`, and
//! [`VynkorClient::send_action`] itself requires "drive request/response
//! traffic from a single task" (it loops on `recv_timeout`, discarding
//! anything that doesn't match its own `action_id` — two tasks calling it on
//! a shared client would race and steal each other's replies).
//!
//! The fix is the same multiplexing trick every RPC client over one
//! connection uses: exactly one task (the loop in `main.rs`) owns the
//! client's read+write halves. Handler tasks get an [`OutboundHandle`]
//! instead — a cheap `Clone` wrapping an `mpsc::Sender<OutboundCall>` — and
//! `.send_action(...)` on it just asks the loop task to do the real send and
//! hands back a `oneshot::Receiver` for the eventual reply. The loop task
//! matches inbound `ActionResponse`s against a `pending` map by
//! `action_id`, exactly like [`VynkorClient::send_action`] did internally,
//! except now dozens of calls can be in flight at once instead of one.

use std::future::Future;

use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::ActionResponse;
use vynkor_sdk::{VynkorClient, VynkorError};

/// One outbound call a handler task wants the loop task to perform.
pub struct OutboundCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<ActionResponse, VynkorError>>,
}

/// Abstraction over "make an outbound action call and await the response."
/// [`VynkorClient`] implements it directly (startup, before the concurrent
/// loop begins); [`OutboundHandle`] implements it for handler tasks that
/// never see the real client. `&mut self` even on the handle (which needs no
/// mutation) so both impls share one method signature.
pub trait ActionCaller: Send {
    fn call_action(
        &mut self,
        action: &str,
        params_json: &[u8],
        timeout_ms: u32,
    ) -> impl Future<Output = Result<ActionResponse, VynkorError>> + Send;
}

impl ActionCaller for VynkorClient {
    fn call_action(
        &mut self,
        action: &str,
        params_json: &[u8],
        timeout_ms: u32,
    ) -> impl Future<Output = Result<ActionResponse, VynkorError>> + Send {
        self.send_action(action, params_json, timeout_ms)
    }
}

/// Cheaply-cloneable handle spawned handler tasks use in place of a client.
#[derive(Clone)]
pub struct OutboundHandle {
    tx: mpsc::Sender<OutboundCall>,
}

impl OutboundHandle {
    pub fn new(tx: mpsc::Sender<OutboundCall>) -> Self {
        Self { tx }
    }
}

impl ActionCaller for OutboundHandle {
    fn call_action(
        &mut self,
        action: &str,
        params_json: &[u8],
        timeout_ms: u32,
    ) -> impl Future<Output = Result<ActionResponse, VynkorError>> + Send {
        let tx = self.tx.clone();
        let action = action.to_string();
        let params_json = params_json.to_vec();
        async move {
            let (reply, rx) = oneshot::channel();
            tx.send(OutboundCall { action, params_json, timeout_ms, reply })
                .await
                .map_err(|_| VynkorError::Internal("ai: outbound loop closed".into()))?;
            rx.await.map_err(|_| VynkorError::Internal("ai: outbound call dropped".into()))?
        }
    }
}
