//! Per-provider request building and response parsing. Each adapter
//! translates between `ai`'s normalized shapes and the provider's own wire
//! format; the actual HTTP send happens in `network`'s `http_request`
//! action (see `crate::handler`), not here.

pub mod anthropic;
pub mod openai_compat;

use std::collections::HashMap;

use crate::request::ChatCompletionParams;

/// Mirrors `network`'s `http_request` action params — built by an adapter,
/// serialized as-is into the `ActionRequest.params_json` sent to `network`.
#[derive(Debug, serde::Serialize)]
pub struct HttpRequestJson {
    pub method: &'static str,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub timeout_ms: u64,
    /// Retries for transient failures (429/5xx), executed by `network`
    /// itself with exponential backoff. Defaults live in
    /// [`crate::request`] and are clamped there.
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default)]
    pub retry_backoff_ms: u64,
}

/// One tool invocation requested by the model. `arguments_json` is the raw
/// arguments object serialized to a string (openai's native shape) so the
/// caller can parse it against its own schema.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

/// Normalized completion result — the shape `ai` returns to its own
/// callers in `ActionResponse.data_json`, regardless of provider.
/// `tool_calls` is omitted when empty so plain-text responses keep the
/// exact pre-tools JSON shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ChatResult {
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: String,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// `Send + Sync` so `&dyn Provider` can be held across an `.await` inside a
/// `tokio::spawn`ed handler task (the concurrent loop in `main.rs` spawns
/// one task per inbound request).
pub trait Provider: Send + Sync {
    /// Build the `network` `http_request` params for this completion call.
    /// `api_key` is the resolved secret value (never logged, never echoed
    /// back in any error).
    fn build_http_request(&self, params: &ChatCompletionParams, api_key: &str) -> HttpRequestJson;

    /// Parse the provider's raw HTTP response body into the normalized
    /// result. Called only on a 2xx status — non-2xx is handled by
    /// `crate::handler` before this is reached.
    fn parse_response(&self, body: &[u8]) -> Result<ChatResult, String>;
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EmbeddingResult {
    pub embedding: Vec<f32>,
    pub dim: usize,
    pub model: String,
    pub usage: Usage,
}

/// See [`Provider`]'s doc comment for why this needs `Send + Sync`.
pub trait EmbeddingProvider: Send + Sync {
    fn build_embedding_request(
        &self,
        params: &crate::request::EmbeddingParams,
        api_key: &str,
    ) -> HttpRequestJson;
    fn parse_embedding_response(&self, body: &[u8]) -> Result<EmbeddingResult, String>;
}
