//! Shared plumbing for the `automations` plugin: the channel-fronted RPC
//! proxy tasks use to reach `database` and rule targets through the serve
//! loop's single reader.

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

pub mod request;
pub mod store;
pub mod template;

/// One pending kernel-routed call handed from a task to the serve loop,
/// which sends it and correlates the `ActionResponse` by `action_id`.
pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// Cloneable handle for kernel-routed actions (`database`, dispatched rule
/// targets). Every round-trip goes through the serve loop's single `recv()`
/// point, so outbound calls can never discard inbound frames.
#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self { tx }
    }

    pub async fn call(
        &self,
        action: &str,
        params_json: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<Value, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RpcCall { action: action.to_string(), params_json, timeout_ms, reply })
            .await
            .map_err(|_| format!("{action} aborted: serve loop is shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("{action} aborted: serve loop is shutting down")),
            Err(_) => Err(format!("{action} timed out after {effective} ms")),
        }
    }

    pub async fn call_action(&self, action: &str, params: Value, timeout_ms: u32) -> Result<Value, String> {
        let params_json =
            serde_json::to_vec(&params).map_err(|e| format!("failed to encode {action} params: {e}"))?;
        self.call(action, params_json, timeout_ms).await
    }
}
