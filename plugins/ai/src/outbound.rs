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

/// What a handler task can ask the loop task to do on the shared client.
pub enum Outbound {
    Call(OutboundCall),
    /// CD-03: an `ActionRequest{streaming: true}`; the loop routes the
    /// session's frames into `events` and reports the minted id on `opened`.
    OpenStream {
        action: String,
        params_json: Vec<u8>,
        timeout_ms: u32,
        events: mpsc::UnboundedSender<UpstreamEvent>,
        opened: oneshot::Sender<Result<String, VynkorError>>,
    },
    /// Stop an accepted upstream session (`SessionClose`).
    CloseStream {
        action_id: String,
    },
}

/// One frame of an outbound streaming session, as routed by the loop.
#[derive(Debug, PartialEq, Eq)]
pub enum UpstreamEvent {
    /// The provider's accepting `ActionResponse{OK}` (`data_json`).
    Accepted(Vec<u8>),
    Chunk(Vec<u8>),
    /// The provider's `SessionClose` — normal end.
    Done,
    /// Error `ActionResponse` or kernel `ActionStreamAbort`.
    Failed(String),
}

/// Handler-side end of an outbound streaming session. Unbounded on
/// purpose: the loop must never block on a slow handler (the handler may
/// itself be waiting on the loop to forward its own output).
pub struct UpstreamStream {
    pub action_id: String,
    pub events: mpsc::UnboundedReceiver<UpstreamEvent>,
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
    tx: mpsc::Sender<Outbound>,
}

impl OutboundHandle {
    pub fn new(tx: mpsc::Sender<Outbound>) -> Self {
        Self { tx }
    }

    /// Open a streaming call (CD-03). Returns once the request is on the
    /// wire; the provider's acceptance arrives as the first event.
    pub async fn open_stream(
        &self,
        action: &str,
        params_json: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<UpstreamStream, VynkorError> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (opened, rx) = oneshot::channel();
        self.tx
            .send(Outbound::OpenStream {
                action: action.to_string(),
                params_json,
                timeout_ms,
                events: events_tx,
                opened,
            })
            .await
            .map_err(|_| VynkorError::Internal("ai: outbound loop closed".into()))?;
        let action_id = rx
            .await
            .map_err(|_| VynkorError::Internal("ai: outbound call dropped".into()))??;
        Ok(UpstreamStream { action_id, events })
    }

    /// Best effort: a loop that is gone has no session left to close.
    pub async fn close_stream(&self, action_id: &str) {
        let _ = self
            .tx
            .send(Outbound::CloseStream {
                action_id: action_id.to_string(),
            })
            .await;
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
            tx.send(Outbound::Call(OutboundCall {
                action,
                params_json,
                timeout_ms,
                reply,
            }))
            .await
            .map_err(|_| VynkorError::Internal("ai: outbound loop closed".into()))?;
            rx.await
                .map_err(|_| VynkorError::Internal("ai: outbound call dropped".into()))?
        }
    }
}
