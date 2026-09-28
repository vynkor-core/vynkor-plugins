//! Parse + validate the JSON body of a `chat_completion` `ActionRequest`.

/// Hard ceiling on `timeout_ms`; matches `network`'s own cap so a
/// `chat_completion` call can't outlive the HTTP request it wraps.
pub const MAX_TIMEOUT_MS: u64 = 30_000;

/// Default `max_tokens` when the caller omits it.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Hard ceiling on `max_tokens`. Clamped, never rejected.
pub const MAX_MAX_TOKENS: u32 = 8192;

/// Default `max_retries` forwarded to `network`'s `http_request`. Unlike
/// `network` itself (opt-in, default 0), LLM providers rate-limit often
/// enough that a small retry budget is the sane default here — see ROADMAP.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Hard ceiling on `max_retries`; matches `network`'s own cap.
pub const MAX_RETRIES: u32 = 5;

/// Default per-attempt retry backoff (doubling each attempt). Longer than
/// `network`'s 200 ms default on purpose: provider 429s need real cooling-
/// off time, not an instant second hammer.
pub const DEFAULT_RETRY_BACKOFF_MS: u64 = 1000;

/// Hard ceiling on `retry_backoff_ms`; matches `network`'s own cap.
pub const MAX_RETRY_BACKOFF_MS: u64 = 5000;

/// Image MIME types accepted in `image` content blocks. Matches the set
/// both providers support natively.
pub const ALLOWED_IMAGE_MIME_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Decoded-size ceiling per attached image (providers reject anything much
/// larger anyway; failing here gives a clearer error than HTTP 413).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Max `image` blocks per single message.
pub const MAX_IMAGES_PER_MESSAGE: usize = 8;

/// Max `tools` entries per `chat_completion` call.
pub const MAX_TOOLS: usize = 64;

/// Serialized-size ceiling on one tool's `input_schema`.
pub const MAX_TOOL_SCHEMA_BYTES: usize = 32 * 1024;

pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// Operator-supplied allowlist of env var names a caller's `api_key_env`
/// may name. Comma-separated, exact (case-sensitive) match. Default-deny:
/// unset or empty means no `api_key_env` value is accepted — a caller
/// could otherwise name *any* environment variable in the `ai` process
/// (an unrelated secret, not just a provider key) and have its value sent
/// straight into an outbound request header to a caller-controlled
/// `base_url`, exfiltrating it.
pub const ALLOWED_KEY_ENVS_ENV: &str = "AI_PLUGIN_ALLOWED_KEY_ENVS";

/// Parse [`ALLOWED_KEY_ENVS_ENV`]'s raw value into the set of permitted
/// `api_key_env` names.
pub fn parse_allowed_key_envs(raw: &str) -> std::collections::HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// True if `name` is permitted as an `api_key_env` value, per the
/// operator's [`ALLOWED_KEY_ENVS_ENV`] allowlist.
pub fn is_allowed_key_env(name: &str, allowed: &std::collections::HashSet<String>) -> bool {
    allowed.contains(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub content: String,
    /// Images attached to this message, in order. Empty for plain-text
    /// messages; providers without vision support reject them server-side.
    pub images: Vec<ImageBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBlock {
    /// One of [`ALLOWED_IMAGE_MIME_TYPES`].
    pub mime_type: String,
    /// Base64 payload (no `data:` prefix) — decoded form is validated
    /// against [`MAX_IMAGE_BYTES`] at parse time.
    pub data_base64: String,
}

/// One native tool definition passed through to the provider. `input_schema`
/// is a JSON Schema object describing the tool's parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

fn validate_image_block(block: &ImageBlock) -> Result<(), String> {
    if !ALLOWED_IMAGE_MIME_TYPES.contains(&block.mime_type.as_str()) {
        return Err(format!(
            "unsupported image mime_type '{}' (allowed: {})",
            block.mime_type,
            ALLOWED_IMAGE_MIME_TYPES.join(", ")
        ));
    }
    if block.data_base64.is_empty() {
        return Err("image data_base64 must not be empty".to_string());
    }
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&block.data_base64)
        .map_err(|e| format!("image data_base64 is not valid base64: {e}"))?;
    if decoded.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image too large: {} decoded bytes (max {MAX_IMAGE_BYTES})",
            decoded.len()
        ));
    }
    Ok(())
}

/// Flatten a `messages[].content` value into text + images. Accepts a plain
/// string or an array of typed blocks:
/// `{"type":"text","text":...}` and `{"type":"image","mime_type":...,"data_base64":...}`.
fn parse_content_value(value: serde_json::Value) -> Result<(String, Vec<ImageBlock>), String> {
    match value {
        serde_json::Value::String(s) => Ok((s, Vec::new())),
        serde_json::Value::Array(blocks) => {
            let mut texts: Vec<String> = Vec::new();
            let mut images = Vec::new();
            for block in blocks {
                let obj = block
                    .as_object()
                    .ok_or("content blocks must be JSON objects")?;
                match obj.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        let text = obj
                            .get("text")
                            .and_then(|t| t.as_str())
                            .ok_or("text block requires a string 'text' field")?;
                        texts.push(text.to_string());
                    }
                    Some("image") => {
                        let mime_type = obj
                            .get("mime_type")
                            .and_then(|t| t.as_str())
                            .ok_or("image block requires a string 'mime_type' field")?;
                        let data_base64 = obj
                            .get("data_base64")
                            .and_then(|d| d.as_str())
                            .ok_or("image block requires a string 'data_base64' field")?;
                        let block = ImageBlock {
                            mime_type: mime_type.to_string(),
                            data_base64: data_base64.to_string(),
                        };
                        validate_image_block(&block)?;
                        images.push(block);
                    }
                    other => {
                        return Err(format!("unsupported content block type: {:?}", other));
                    }
                }
            }
            if images.len() > MAX_IMAGES_PER_MESSAGE {
                return Err(format!(
                    "too many images in one message: {} (max {MAX_IMAGES_PER_MESSAGE})",
                    images.len()
                ));
            }
            Ok((texts.join("\n"), images))
        }
        _ => Err("messages[].content must be a string or an array of content blocks".to_string()),
    }
}

fn validate_tools(tools: &[ToolSpec]) -> Result<(), String> {
    if tools.len() > MAX_TOOLS {
        return Err(format!("too many tools: {} (max {MAX_TOOLS})", tools.len()));
    }
    let mut seen = std::collections::HashSet::new();
    for tool in tools {
        if tool.name.trim().is_empty() {
            return Err("tools[].name must not be empty".to_string());
        }
        if !seen.insert(tool.name.clone()) {
            return Err(format!("duplicate tool name: {}", tool.name));
        }
        if !tool.input_schema.is_object() {
            return Err(format!(
                "tool '{}' input_schema must be a JSON object",
                tool.name
            ));
        }
        let schema_len = serde_json::to_vec(&tool.input_schema)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if schema_len > MAX_TOOL_SCHEMA_BYTES {
            return Err(format!(
                "tool '{}' input_schema too large: {schema_len} bytes (max {MAX_TOOL_SCHEMA_BYTES})",
                tool.name
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatCompletionParams {
    pub provider: Provider,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    pub timeout_ms: u64,
    /// Native tool definitions forwarded to the provider; empty = none.
    pub tools: Vec<ToolSpec>,
    /// Retries forwarded to `network`'s `http_request` (429/5xx only).
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
    /// Named agent whose model + system prompt are resolved by the handler
    /// from the database. When set, `model`/`provider`/`base_url`/
    /// `api_key_env` may be empty (they are filled in by the handler).
    pub agent_id: Option<String>,
    /// System prompt resolved from the agent profile (never caller-supplied).
    pub system_prompt: Option<String>,
}

/// Parse and validate `params_json` for the `chat_completion` action.
/// Returns a human-readable error message on any validation failure —
/// caller maps that straight into `ActionResponse.error`.
pub fn parse_request(params_json: &[u8]) -> Result<ChatCompletionParams, String> {
    #[derive(serde::Deserialize)]
    struct RawMessage {
        role: String,
        content: serde_json::Value,
    }

    #[derive(serde::Deserialize)]
    struct RawTool {
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default = "default_tool_schema")]
        input_schema: serde_json::Value,
    }

    fn default_tool_schema() -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    #[derive(serde::Deserialize)]
    struct Raw {
        provider: Option<String>,
        base_url: Option<String>,
        model: Option<String>,
        api_key_env: Option<String>,
        messages: Option<Vec<RawMessage>>,
        #[serde(default)]
        tools: Vec<RawTool>,
        max_tokens: Option<u32>,
        timeout_ms: Option<u64>,
        max_retries: Option<u32>,
        retry_backoff_ms: Option<u64>,
        agent_id: Option<String>,
    }

    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let agent_id = raw.agent_id.filter(|s| !s.is_empty());
    // no provider: the endpoint comes from the host's model table (an
    // agent_id's model, or a model id from list_models) — resolve_params
    // fills it in or rejects an id the host does not know
    let from_db = agent_id.is_some() || raw.provider.is_none();

    let provider = match raw.provider {
        Some(p) => match p.as_str() {
            "anthropic" => Provider::Anthropic,
            "openai" => Provider::OpenAi,
            other => return Err(format!("unsupported provider: {other}")),
        },
        None => Provider::OpenAi,
    };

    let base_url = match (raw.base_url, provider) {
        (Some(u), _) if !u.is_empty() => u,
        (_, Provider::OpenAi) if from_db => String::new(),
        (_, Provider::Anthropic) => DEFAULT_ANTHROPIC_BASE_URL.to_string(),
        (_, Provider::OpenAi) => return Err("missing required field: base_url".to_string()),
    };

    let model = raw.model.unwrap_or_default();
    if model.is_empty() && agent_id.is_none() {
        return Err("missing required field: model".to_string());
    }

    let api_key_env = raw.api_key_env.unwrap_or_default();
    if api_key_env.is_empty() && !from_db {
        return Err("missing required field: api_key_env".to_string());
    }

    let raw_messages = raw.messages.ok_or("missing required field: messages")?;
    if raw_messages.is_empty() {
        return Err("messages must not be empty".to_string());
    }
    let mut messages = Vec::with_capacity(raw_messages.len());
    for m in raw_messages {
        let (content, images) = parse_content_value(m.content)?;
        messages.push(Message {
            role: m.role,
            content,
            images,
        });
    }

    let tools: Vec<ToolSpec> = raw
        .tools
        .into_iter()
        .map(|t| ToolSpec {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
        })
        .collect();
    validate_tools(&tools)?;

    let max_tokens = raw
        .max_tokens
        .unwrap_or(DEFAULT_MAX_TOKENS)
        .min(MAX_MAX_TOKENS);
    let timeout_ms = raw.timeout_ms.unwrap_or(MAX_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
    let max_retries = raw
        .max_retries
        .unwrap_or(DEFAULT_MAX_RETRIES)
        .min(MAX_RETRIES);
    let retry_backoff_ms = raw
        .retry_backoff_ms
        .unwrap_or(DEFAULT_RETRY_BACKOFF_MS)
        .min(MAX_RETRY_BACKOFF_MS);

    Ok(ChatCompletionParams {
        provider,
        base_url,
        model,
        api_key_env,
        messages,
        max_tokens,
        timeout_ms,
        tools,
        max_retries,
        retry_backoff_ms,
        agent_id,
        system_prompt: None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingParams {
    pub provider: Provider,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub input: String,
    pub timeout_ms: u64,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
    pub agent_id: Option<String>,
}

pub fn parse_embedding_request(params_json: &[u8]) -> Result<EmbeddingParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        provider: Option<String>,
        base_url: Option<String>,
        model: Option<String>,
        api_key_env: Option<String>,
        input: Option<String>,
        timeout_ms: Option<u64>,
        max_retries: Option<u32>,
        retry_backoff_ms: Option<u64>,
        agent_id: Option<String>,
    }
    let raw: Raw =
        serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;
    let agent_id = raw.agent_id.filter(|s| !s.is_empty());
    let provider = match raw.provider {
        Some(p) => match p.as_str() {
            "openai" => Provider::OpenAi,
            "anthropic" => return Err("anthropic does not support embeddings".to_string()),
            other => return Err(format!("unsupported provider: {other}")),
        },
        None if agent_id.is_some() => Provider::OpenAi,
        None => return Err("missing required field: provider".to_string()),
    };
    let base_url = match (raw.base_url, provider) {
        (Some(u), _) if !u.is_empty() => u,
        (_, _) if agent_id.is_some() => String::new(),
        (_, Provider::OpenAi) => return Err("missing required field: base_url".to_string()),
        (_, _) => return Err("missing required field: base_url".to_string()),
    };
    let model = raw.model.unwrap_or_default();
    if model.is_empty() && agent_id.is_none() {
        return Err("missing required field: model".to_string());
    }
    let api_key_env = raw.api_key_env.unwrap_or_default();
    if api_key_env.is_empty() && agent_id.is_none() {
        return Err("missing required field: api_key_env".to_string());
    }
    let input = raw.input.ok_or("missing required field: input")?;
    if input.trim().is_empty() {
        return Err("input must not be empty".to_string());
    }
    if input.len() > 10000 {
        return Err("input too long (max 10000)".to_string());
    }
    let timeout_ms = raw.timeout_ms.unwrap_or(MAX_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
    let max_retries = raw
        .max_retries
        .unwrap_or(DEFAULT_MAX_RETRIES)
        .min(MAX_RETRIES);
    let retry_backoff_ms = raw
        .retry_backoff_ms
        .unwrap_or(DEFAULT_RETRY_BACKOFF_MS)
        .min(MAX_RETRY_BACKOFF_MS);
    Ok(EmbeddingParams {
        provider,
        base_url,
        model,
        api_key_env,
        input,
        timeout_ms,
        max_retries,
        retry_backoff_ms,
        agent_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_anthropic_json() -> serde_json::Value {
        serde_json::json!({
            "provider": "anthropic",
            "model": "claude-sonnet-5",
            "api_key_env": "ANTHROPIC_API_KEY",
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    #[test]
    fn accepts_minimal_anthropic_request() {
        let body = valid_anthropic_json().to_string();
        let params = parse_request(body.as_bytes()).unwrap();
        assert_eq!(params.provider, Provider::Anthropic);
        assert_eq!(params.base_url, DEFAULT_ANTHROPIC_BASE_URL);
        assert_eq!(params.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(params.messages.len(), 1);
    }

    #[test]
    fn missing_provider_defers_endpoint_to_the_host() {
        let mut body = valid_anthropic_json();
        body.as_object_mut().unwrap().remove("provider");
        body.as_object_mut().unwrap().remove("api_key_env");
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert!(!params.model.is_empty());
        assert!(params.base_url.is_empty());
        assert!(params.api_key_env.is_empty());
    }

    #[test]
    fn explicit_provider_still_requires_api_key_env() {
        let mut body = valid_anthropic_json();
        body.as_object_mut().unwrap().remove("api_key_env");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("api_key_env"), "error was: {err}");
    }

    #[test]
    fn agent_id_allows_omitting_provider_and_model() {
        let body = serde_json::json!({
            "agent_id": "code",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.agent_id.as_deref(), Some("code"));
        assert!(params.model.is_empty());
        assert!(params.base_url.is_empty());
        assert!(params.api_key_env.is_empty());
    }

    #[test]
    fn rejects_unsupported_provider() {
        let mut body = valid_anthropic_json();
        body["provider"] = "gemini".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("unsupported provider"), "error was: {err}");
    }

    #[test]
    fn openai_requires_base_url() {
        let mut body = valid_anthropic_json();
        body["provider"] = "openai".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("base_url"), "error was: {err}");
    }

    #[test]
    fn openai_accepts_explicit_base_url() {
        let mut body = valid_anthropic_json();
        body["provider"] = "openai".into();
        body["base_url"] = "http://localhost:11434/v1".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.provider, Provider::OpenAi);
        assert_eq!(params.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn rejects_missing_messages() {
        let mut body = valid_anthropic_json();
        body.as_object_mut().unwrap().remove("messages");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("messages"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_messages() {
        let mut body = valid_anthropic_json();
        body["messages"] = serde_json::json!([]);
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("messages"), "error was: {err}");
    }

    #[test]
    fn clamps_max_tokens_above_cap() {
        let mut body = valid_anthropic_json();
        body["max_tokens"] = 999_999.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.max_tokens, MAX_MAX_TOKENS);
    }

    #[test]
    fn rejects_missing_api_key_env() {
        let mut body = valid_anthropic_json();
        body.as_object_mut().unwrap().remove("api_key_env");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("api_key_env"), "error was: {err}");
    }

    #[test]
    fn allowed_key_envs_empty_by_default() {
        assert!(parse_allowed_key_envs("").is_empty());
    }

    #[test]
    fn allowed_key_envs_parses_comma_list() {
        let allowed = parse_allowed_key_envs("ANTHROPIC_API_KEY, OPENAI_API_KEY ,,");
        assert!(is_allowed_key_env("ANTHROPIC_API_KEY", &allowed));
        assert!(is_allowed_key_env("OPENAI_API_KEY", &allowed));
        assert_eq!(allowed.len(), 2);
    }

    #[test]
    fn is_allowed_key_env_rejects_unlisted_name() {
        let allowed = parse_allowed_key_envs("ANTHROPIC_API_KEY");
        assert!(!is_allowed_key_env("AWS_SECRET_ACCESS_KEY", &allowed));
    }

    #[test]
    fn is_allowed_key_env_is_case_sensitive() {
        let allowed = parse_allowed_key_envs("ANTHROPIC_API_KEY");
        assert!(!is_allowed_key_env("anthropic_api_key", &allowed));
    }

    #[test]
    fn is_allowed_key_env_rejects_everything_when_empty() {
        let allowed = parse_allowed_key_envs("");
        assert!(!is_allowed_key_env("ANTHROPIC_API_KEY", &allowed));
    }

    #[test]
    fn accepts_plain_string_content_with_default_retries() {
        let params = parse_request(valid_anthropic_json().to_string().as_bytes()).unwrap();
        assert_eq!(params.messages[0].images, Vec::new());
        assert_eq!(params.messages[0].content, "hi");
        assert_eq!(params.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(params.retry_backoff_ms, DEFAULT_RETRY_BACKOFF_MS);
        assert!(params.tools.is_empty());
    }

    #[test]
    fn parses_content_blocks_with_image() {
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "claude-sonnet-5",
            "api_key_env": "ANTHROPIC_API_KEY",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this?"},
                    {"type": "image", "mime_type": "image/png", "data_base64": "aGVsbG8="}
                ]
            }]
        });
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.messages[0].content, "what is this?");
        assert_eq!(params.messages[0].images.len(), 1);
        assert_eq!(params.messages[0].images[0].mime_type, "image/png");
    }

    #[test]
    fn joins_multiple_text_blocks() {
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "one"},
                {"type": "text", "text": "two"}
            ]}]
        });
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.messages[0].content, "one\ntwo");
    }

    #[test]
    fn rejects_unsupported_image_mime() {
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": [
                {"type": "image", "mime_type": "image/bmp", "data_base64": "aGk="}
            ]}]
        });
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("unsupported image mime_type"), "error was: {err}");
    }

    #[test]
    fn rejects_invalid_base64_image_data() {
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": [
                {"type": "image", "mime_type": "image/png", "data_base64": "!!!not base64!!!"}
            ]}]
        });
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("not valid base64"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_image() {
        use base64::Engine;
        let big = vec![0u8; MAX_IMAGE_BYTES + 1];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&big);
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": [
                {"type": "image", "mime_type": "image/png", "data_base64": encoded}
            ]}]
        });
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("too large"), "error was: {err}");
    }

    #[test]
    fn rejects_too_many_images_in_one_message() {
        let blocks: Vec<serde_json::Value> = (0..=MAX_IMAGES_PER_MESSAGE)
            .map(|_| {
                serde_json::json!({"type": "image", "mime_type": "image/png", "data_base64": "aGk="})
            })
            .collect();
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": blocks}]
        });
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("too many images"), "error was: {err}");
    }

    #[test]
    fn rejects_unknown_content_block_type() {
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": [{"type": "video", "url": "x"}]}]
        });
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("unsupported content block type"), "error was: {err}");
    }

    #[test]
    fn rejects_non_string_non_array_content() {
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": 42}]
        });
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("string or an array"), "error was: {err}");
    }

    #[test]
    fn parses_tools_and_defaults_schema() {
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "claude-sonnet-5",
            "api_key_env": "ANTHROPIC_API_KEY",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "launch", "description": "Launch an app"},
                {"name": "sys_info", "input_schema": {"type": "object", "properties": {}}}
            ]
        });
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.tools.len(), 2);
        assert_eq!(params.tools[0].name, "launch");
        assert_eq!(params.tools[0].input_schema["type"], "object");
        assert_eq!(params.tools[1].description, "");
    }

    #[test]
    fn rejects_duplicate_tool_names() {
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "launch"},
                {"name": "launch"}
            ]
        });
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("duplicate tool name"), "error was: {err}");
    }

    #[test]
    fn rejects_blank_tool_name_and_non_object_schema() {
        let base = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let mut bad_name = base.clone();
        bad_name["tools"] = serde_json::json!([{"name": " "}]);
        let err = parse_request(bad_name.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("name must not be empty"), "error was: {err}");

        let mut bad_schema = base;
        bad_schema["tools"] = serde_json::json!([{"name": "t", "input_schema": []}]);
        let err = parse_request(bad_schema.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("must be a JSON object"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_tool_schema() {
        let mut schema = serde_json::json!({"type": "object", "properties": {}});
        for i in 0..2000 {
            schema["properties"][format!("prop{i}")] =
                serde_json::Value::String("x".repeat(64));
        }
        let body = serde_json::json!({
            "provider": "anthropic",
            "model": "m",
            "api_key_env": "K",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "t", "input_schema": schema}]
        });
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("too large"), "error was: {err}");
    }

    #[test]
    fn clamps_retry_params() {
        let mut body = valid_anthropic_json();
        body["max_retries"] = 99.into();
        body["retry_backoff_ms"] = 999_999.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.max_retries, MAX_RETRIES);
        assert_eq!(params.retry_backoff_ms, MAX_RETRY_BACKOFF_MS);
    }
}
