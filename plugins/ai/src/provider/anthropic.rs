//! Anthropic Messages API adapter (`POST {base_url}/v1/messages`).

use std::collections::HashMap;

use super::{ChatResult, HttpRequestJson, Provider, Usage};
use crate::request::ChatCompletionParams;

/// Anthropic API version pinned in the `anthropic-version` header — see
/// https://docs.anthropic.com/en/api/versioning.
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider;

fn message_json(m: &crate::request::Message) -> serde_json::Value {
    if m.images.is_empty() {
        return serde_json::json!({"role": m.role, "content": m.content});
    }
    let mut blocks = Vec::with_capacity(1 + m.images.len());
    if !m.content.is_empty() {
        blocks.push(serde_json::json!({"type": "text", "text": m.content}));
    }
    for img in &m.images {
        blocks.push(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": img.mime_type,
                "data": img.data_base64,
            }
        }));
    }
    serde_json::json!({"role": m.role, "content": blocks})
}

impl Provider for AnthropicProvider {
    fn build_http_request(&self, params: &ChatCompletionParams, api_key: &str) -> HttpRequestJson {
        let url = format!("{}/v1/messages", params.base_url.trim_end_matches('/'));

        let mut messages: Vec<serde_json::Value> =
            params.messages.iter().map(message_json).collect();
        let cache_enabled = std::env::var("AI_PROMPT_CACHE")
            .map(|v| !v.trim().eq_ignore_ascii_case("off") && v.trim() != "0" && !v.trim().eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        if cache_enabled && !messages.is_empty() {
            if let Some(first) = messages.get_mut(0) {
                if let Some(content) = first.get("content").and_then(|c| c.as_str()) {
                    if content.len() > 800 {
                        let cached = serde_json::json!([{"type": "text", "text": content, "cache_control": {"type": "ephemeral"}}]);
                        first["content"] = cached;
                    }
                }
            }
        }

        let mut body = serde_json::json!({
            "model": params.model,
            "max_tokens": params.max_tokens,
            "messages": messages,
        });
        if !params.tools.is_empty() {
            let mut tools: Vec<serde_json::Value> = params
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            if cache_enabled && !tools.is_empty() {
                if let Some(last) = tools.last_mut() {
                    last["cache_control"] = serde_json::json!({"type": "ephemeral"});
                }
            }
            body["tools"] = serde_json::Value::Array(tools);
        }
        if let Some(system) = params.system_prompt.as_deref().filter(|s| !s.is_empty()) {
            if cache_enabled {
                body["system"] = serde_json::json!([{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}]);
            } else {
                body["system"] = system.into();
            }
        }
        let body = body.to_string();

        let mut headers = HashMap::new();
        headers.insert("x-api-key".to_string(), api_key.to_string());
        headers.insert(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        );
        headers.insert("content-type".to_string(), "application/json".to_string());

        HttpRequestJson {
            method: "POST",
            url,
            headers,
            body,
            timeout_ms: params.timeout_ms,
            max_retries: params.max_retries,
            retry_backoff_ms: params.retry_backoff_ms,
        }
    }

    fn parse_response(&self, body: &[u8]) -> Result<ChatResult, String> {
        #[derive(serde::Deserialize)]
        struct AnthropicUsage {
            #[serde(default)]
            input_tokens: u64,
            #[serde(default)]
            output_tokens: u64,
        }
        #[derive(serde::Deserialize)]
        struct Response {
            content: Vec<serde_json::Value>,
            #[serde(default)]
            stop_reason: Option<String>,
            usage: AnthropicUsage,
        }

        let resp: Response = serde_json::from_slice(body)
            .map_err(|e| format!("malformed anthropic response: {e}"))?;
        if resp.content.is_empty() {
            return Err("anthropic response has no content blocks".to_string());
        }

        let mut texts: Vec<&str> = Vec::new();
        let mut tool_calls = Vec::new();
        for block in &resp.content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        texts.push(text);
                    }
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                    let arguments = block.get("input").cloned().unwrap_or(serde_json::json!({}));
                    tool_calls.push(super::ToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        arguments_json: arguments.to_string(),
                    });
                }
                _ => {}
            }
        }
        if texts.is_empty() && tool_calls.is_empty() {
            return Err("anthropic response has no usable content blocks".to_string());
        }

        Ok(ChatResult {
            content: texts.join("\n"),
            tool_calls,
            stop_reason: resp.stop_reason.unwrap_or_default(),
            usage: Usage {
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{
        ImageBlock, Message, Provider as ReqProvider, ToolSpec, DEFAULT_MAX_RETRIES,
        DEFAULT_RETRY_BACKOFF_MS,
    };

    fn params() -> ChatCompletionParams {
        ChatCompletionParams {
            provider: ReqProvider::Anthropic,
            base_url: "https://api.anthropic.com".to_string(),
            model: "claude-sonnet-5".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: "hi".to_string(),
                images: Vec::new(),
            }],
            max_tokens: 1024,
            timeout_ms: 30_000,
            tools: Vec::new(),
            max_retries: DEFAULT_MAX_RETRIES,
            retry_backoff_ms: DEFAULT_RETRY_BACKOFF_MS,
            agent_id: None,
            system_prompt: None,
        }
    }

    #[test]
    fn builds_request_with_auth_header_and_no_leaked_key_in_url() {
        let req = AnthropicProvider.build_http_request(&params(), "sk-secret");
        assert_eq!(req.url, "https://api.anthropic.com/v1/messages");
        assert_eq!(req.headers.get("x-api-key").unwrap(), "sk-secret");
        assert!(!req.url.contains("sk-secret"));
        assert!(req.body.contains("claude-sonnet-5"));
        assert_eq!(req.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(req.retry_backoff_ms, DEFAULT_RETRY_BACKOFF_MS);
    }

    #[test]
    fn image_messages_become_base64_source_blocks() {
        let mut p = params();
        p.messages[0].images.push(ImageBlock {
            mime_type: "image/png".to_string(),
            data_base64: "aGVsbG8=".to_string(),
        });
        let req = AnthropicProvider.build_http_request(&p, "k");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "aGVsbG8=");
    }

    #[test]
    fn plain_text_messages_keep_string_content() {
        let req = AnthropicProvider.build_http_request(&params(), "k");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn tools_are_forwarded_with_input_schema() {
        let mut p = params();
        p.tools.push(ToolSpec {
            name: "launch".to_string(),
            description: "Launch an app".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        });
        let req = AnthropicProvider.build_http_request(&p, "k");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["tools"][0]["name"], "launch");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn parses_valid_response() {
        let body = serde_json::json!({
            "content": [{"type": "text", "text": "hello there"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        })
        .to_string();
        let result = AnthropicProvider.parse_response(body.as_bytes()).unwrap();
        assert_eq!(result.content, "hello there");
        assert_eq!(result.stop_reason, "end_turn");
        assert_eq!(result.usage.input_tokens, 5);
        assert_eq!(result.usage.output_tokens, 3);
        assert!(result.tool_calls.is_empty());
    }

    #[test]
    fn joins_multiple_text_blocks_and_extracts_tool_use() {
        let body = serde_json::json!({
            "content": [
                {"type": "text", "text": "Launching."},
                {"type": "tool_use", "id": "toolu_1", "name": "launch", "input": {"app_id": "firefox"}},
                {"type": "text", "text": "Done."}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        })
        .to_string();
        let result = AnthropicProvider.parse_response(body.as_bytes()).unwrap();
        assert_eq!(result.content, "Launching.\nDone.");
        assert_eq!(result.stop_reason, "tool_use");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "toolu_1");
        assert_eq!(result.tool_calls[0].name, "launch");
        assert_eq!(
            result.tool_calls[0].arguments_json,
            r#"{"app_id":"firefox"}"#
        );
    }

    #[test]
    fn rejects_response_with_no_content_blocks() {
        let body = serde_json::json!({
            "content": [],
            "usage": {"input_tokens": 1, "output_tokens": 0}
        })
        .to_string();
        let err = AnthropicProvider
            .parse_response(body.as_bytes())
            .unwrap_err();
        assert!(err.contains("no content blocks"), "error was: {err}");
    }

    #[test]
    fn rejects_malformed_json() {
        let err = AnthropicProvider.parse_response(b"not json").unwrap_err();
        assert!(err.contains("malformed"), "error was: {err}");
    }
}
