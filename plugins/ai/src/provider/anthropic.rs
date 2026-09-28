//! Anthropic Messages API adapter (`POST {base_url}/v1/messages`).

use std::collections::HashMap;

use super::{ChatResult, HttpRequestJson, Provider, StreamDelta, StreamParser, ToolCall, Usage};
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
            .map(|v| {
                !v.trim().eq_ignore_ascii_case("off")
                    && v.trim() != "0"
                    && !v.trim().eq_ignore_ascii_case("false")
            })
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

    fn stream_parser(&self) -> Box<dyn StreamParser> {
        Box::<AnthropicStream>::default()
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
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
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

/// One content block as it streams in.
enum Block {
    Text(String),
    Tool {
        id: String,
        name: String,
        json: String,
    },
}

/// Messages-API stream (`message_start` → `content_block_*` →
/// `message_delta` → `message_stop`). Text blocks are joined with `\n`,
/// exactly as [`AnthropicProvider::parse_response`] joins them, and the
/// separator is streamed too so the deltas add up to the final content.
#[derive(Default)]
struct AnthropicStream {
    blocks: Vec<Block>,
    started: bool,
    stop_reason: String,
    input_tokens: u64,
    output_tokens: u64,
}

impl StreamParser for AnthropicStream {
    fn feed(&mut self, data: &str) -> Result<Vec<StreamDelta>, String> {
        let v: serde_json::Value = serde_json::from_str(data)
            .map_err(|e| format!("malformed anthropic stream event: {e}"))?;
        let u64_at = |v: &serde_json::Value, ptr: &str| v.pointer(ptr).and_then(|x| x.as_u64());
        let mut out = Vec::new();
        match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "message_start" => {
                self.started = true;
                self.input_tokens = u64_at(&v, "/message/usage/input_tokens").unwrap_or(0);
                self.output_tokens = u64_at(&v, "/message/usage/output_tokens").unwrap_or(0);
            }
            "content_block_start" => {
                let block = &v["content_block"];
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if self.blocks.iter().any(|b| matches!(b, Block::Text(_))) {
                            out.push(StreamDelta::Text("\n".into()));
                        }
                        let text = block["text"].as_str().unwrap_or_default().to_string();
                        if !text.is_empty() {
                            out.push(StreamDelta::Text(text.clone()));
                        }
                        self.blocks.push(Block::Text(text));
                    }
                    Some("tool_use") => self.blocks.push(Block::Tool {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        json: String::new(),
                    }),
                    // thinking/other blocks: nothing to surface
                    _ => {}
                }
            }
            "content_block_delta" => match (
                v.pointer("/delta/type").and_then(|t| t.as_str()),
                self.blocks.last_mut(),
            ) {
                (Some("text_delta"), Some(Block::Text(acc))) => {
                    let text = v
                        .pointer("/delta/text")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default();
                    acc.push_str(text);
                    out.push(StreamDelta::Text(text.to_string()));
                }
                (Some("input_json_delta"), Some(Block::Tool { json, .. })) => {
                    json.push_str(
                        v.pointer("/delta/partial_json")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default(),
                    );
                }
                _ => {}
            },
            "message_delta" => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(|t| t.as_str()) {
                    self.stop_reason = r.to_string();
                }
                if let Some(n) = u64_at(&v, "/usage/output_tokens") {
                    self.output_tokens = n; // cumulative, not an increment
                }
            }
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(|t| t.as_str())
                    .unwrap_or("unknown error");
                return Err(format!("anthropic stream error: {msg}"));
            }
            // ping, content_block_stop, message_stop
            _ => {}
        }
        Ok(out)
    }

    fn finish(self: Box<Self>) -> Result<ChatResult, String> {
        if !self.started {
            return Err("anthropic stream ended before message_start".into());
        }
        let mut texts = Vec::new();
        let mut tool_calls = Vec::new();
        for block in self.blocks {
            match block {
                Block::Text(t) => texts.push(t),
                Block::Tool { id, name, json } => {
                    // same compact form the buffered path gets from `input`
                    let arguments_json = if json.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        serde_json::from_str::<serde_json::Value>(&json)
                            .map(|v| v.to_string())
                            .unwrap_or(json)
                    };
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments_json,
                    });
                }
            }
        }
        Ok(ChatResult {
            content: texts.join("\n"),
            tool_calls,
            stop_reason: self.stop_reason,
            usage: Usage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
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

    fn feed_all(p: &mut Box<dyn StreamParser>, events: &[&str]) -> String {
        let mut text = String::new();
        for e in events {
            for d in p.feed(e).unwrap() {
                let StreamDelta::Text(t) = d;
                text.push_str(&t);
            }
        }
        text
    }

    #[test]
    fn stream_parser_rebuilds_text_tools_and_usage() {
        let mut p = AnthropicProvider.stream_parser();
        let streamed = feed_all(
            &mut p,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"ping"}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"lights","input":{}}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"on\": "}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"false}"}}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"done"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let r = p.finish().unwrap();
        assert_eq!(r.content, "Hello\ndone");
        assert_eq!(
            streamed, r.content,
            "deltas must add up to the final content"
        );
        assert_eq!(
            r.tool_calls,
            vec![ToolCall {
                id: "tu_1".into(),
                name: "lights".into(),
                arguments_json: r#"{"on":false}"#.into(),
            }]
        );
        assert_eq!(r.stop_reason, "tool_use");
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 9
            }
        );
    }

    #[test]
    fn stream_error_event_and_empty_stream_fail() {
        let mut p = AnthropicProvider.stream_parser();
        let err = p
            .feed(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
            .unwrap_err();
        assert!(err.contains("Overloaded"));
        assert!(AnthropicProvider.stream_parser().finish().is_err());
    }

    #[test]
    fn stream_request_sets_stream_and_disables_retries() {
        let req = AnthropicProvider.build_stream_request(&params(), "k");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(req.max_retries, 0);
    }
}
