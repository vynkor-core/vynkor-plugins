//! OpenAI-compatible chat completions adapter
//! (`POST {base_url}/chat/completions`) — covers OpenAI, OpenRouter, Ollama,
//! and any other self-hosted gateway that speaks the same wire shape.

use std::collections::HashMap;

use super::{
    ChatResult, EmbeddingProvider, EmbeddingResult, HttpRequestJson, Provider, StreamDelta,
    StreamParser, ToolCall, Usage,
};
use crate::request::{ChatCompletionParams, EmbeddingParams};

pub struct OpenAiCompatProvider;

fn message_json(m: &crate::request::Message) -> serde_json::Value {
    if m.images.is_empty() {
        return serde_json::json!({"role": m.role, "content": m.content});
    }
    let mut parts = Vec::with_capacity(1 + m.images.len());
    if !m.content.is_empty() {
        parts.push(serde_json::json!({"type": "text", "text": m.content}));
    }
    for img in &m.images {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{};base64,{}", img.mime_type, img.data_base64)}
        }));
    }
    serde_json::json!({"role": m.role, "content": parts})
}

impl Provider for OpenAiCompatProvider {
    fn build_http_request(&self, params: &ChatCompletionParams, api_key: &str) -> HttpRequestJson {
        let url = format!("{}/chat/completions", params.base_url.trim_end_matches('/'));

        let mut messages: Vec<serde_json::Value> =
            params.messages.iter().map(message_json).collect();
        if let Some(system) = params.system_prompt.as_deref().filter(|s| !s.is_empty()) {
            messages.insert(0, serde_json::json!({"role": "system", "content": system}));
        }

        let mut body = serde_json::json!({
            "model": params.model,
            "max_tokens": params.max_tokens,
            "messages": messages,
        });
        if !params.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                params
                    .tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.input_schema,
                            }
                        })
                    })
                    .collect(),
            );
        }
        let body = body.to_string();

        let mut headers = HashMap::new();
        // Omitted when the resolved key is empty (e.g. a local Ollama
        // instance with no auth) rather than sending `Bearer ` with an
        // empty token.
        if !api_key.is_empty() {
            headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        }
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

    /// Without `include_usage` the stream carries no token counts at all.
    fn stream_body_fields(&self) -> Vec<(&'static str, serde_json::Value)> {
        vec![("stream_options", serde_json::json!({"include_usage": true}))]
    }

    fn stream_parser(&self) -> Box<dyn StreamParser> {
        Box::<OpenAiStream>::default()
    }

    fn parse_response(&self, body: &[u8]) -> Result<ChatResult, String> {
        /// Providers disagree on null vs omitted: mimo-v2.5 sends an explicit
        /// `"tool_calls": null` on plain-text replies — serde(default) alone
        /// rejects that, so nulls must fold into the default value.
        fn null_to_seq<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
        where
            D: serde::Deserializer<'de>,
            T: serde::Deserialize<'de>,
        {
            Ok(<Option<Vec<T>> as serde::Deserialize>::deserialize(d)?.unwrap_or_default())
        }
        #[derive(serde::Deserialize)]
        struct RawToolCallFunction {
            #[serde(default)]
            name: String,
            #[serde(default)]
            arguments: String,
        }
        #[derive(serde::Deserialize)]
        struct RawToolCall {
            #[serde(default)]
            id: String,
            #[serde(default)]
            function: Option<RawToolCallFunction>,
        }
        #[derive(serde::Deserialize)]
        struct ResponseMessage {
            #[serde(default)]
            content: Option<String>,
            #[serde(default, deserialize_with = "null_to_seq")]
            tool_calls: Vec<RawToolCall>,
        }
        #[derive(serde::Deserialize)]
        struct Choice {
            message: ResponseMessage,
            #[serde(default)]
            finish_reason: Option<String>,
        }
        #[derive(serde::Deserialize, Default)]
        struct OpenAiUsage {
            #[serde(default)]
            prompt_tokens: u64,
            #[serde(default)]
            completion_tokens: u64,
        }
        #[derive(serde::Deserialize)]
        struct Response {
            choices: Vec<Choice>,
            #[serde(default)]
            usage: OpenAiUsage,
        }

        let resp: Response = serde_json::from_slice(body)
            .map_err(|e| format!("malformed openai-compatible response: {e}"))?;
        let choice = resp
            .choices
            .into_iter()
            .next()
            .ok_or("openai-compatible response has no choices")?;

        let tool_calls = choice
            .message
            .tool_calls
            .into_iter()
            .filter_map(|tc| {
                tc.function.map(|f| super::ToolCall {
                    id: tc.id,
                    name: f.name,
                    arguments_json: f.arguments,
                })
            })
            .collect();

        Ok(ChatResult {
            content: choice.message.content.unwrap_or_default(),
            tool_calls,
            stop_reason: choice.finish_reason.unwrap_or_default(),
            usage: Usage {
                input_tokens: resp.usage.prompt_tokens,
                output_tokens: resp.usage.completion_tokens,
            },
        })
    }
}

impl EmbeddingProvider for OpenAiCompatProvider {
    fn build_embedding_request(&self, params: &EmbeddingParams, api_key: &str) -> HttpRequestJson {
        let url = format!("{}/embeddings", params.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": params.model,
            "input": params.input,
        })
        .to_string();
        let mut headers = HashMap::new();
        if !api_key.is_empty() {
            headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        }
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

    fn parse_embedding_response(&self, body: &[u8]) -> Result<EmbeddingResult, String> {
        #[derive(serde::Deserialize)]
        struct EmbeddingData {
            embedding: Vec<f32>,
        }
        #[derive(serde::Deserialize, Default)]
        #[allow(dead_code)]
        struct EmbeddingUsage {
            #[serde(default)]
            prompt_tokens: u64,
            #[serde(default)]
            total_tokens: u64,
        }
        #[derive(serde::Deserialize)]
        struct Response {
            data: Vec<EmbeddingData>,
            #[serde(default)]
            model: String,
            #[serde(default)]
            usage: EmbeddingUsage,
        }
        let resp: Response = serde_json::from_slice(body)
            .map_err(|e| format!("malformed openai embedding response: {e}"))?;
        let datum = resp
            .data
            .into_iter()
            .next()
            .ok_or("openai embedding response has no data")?;
        let dim = datum.embedding.len();
        Ok(EmbeddingResult {
            embedding: datum.embedding,
            dim,
            model: resp.model,
            usage: Usage {
                input_tokens: resp.usage.prompt_tokens,
                output_tokens: 0,
            },
        })
    }
}

/// `chat.completion.chunk` stream: `choices[0].delta.{content,tool_calls}`
/// increments, `finish_reason` on the last choice chunk, usage in a final
/// chunk with empty `choices` (when `include_usage` is honored), then
/// `data: [DONE]`. Tool calls arrive keyed by `index`, split across chunks.
#[derive(Default)]
struct OpenAiStream {
    content: String,
    tools: Vec<(String, String, String)>, // id, name, arguments — by index
    started: bool,
    stop_reason: String,
    usage: Usage,
}

impl StreamParser for OpenAiStream {
    fn feed(&mut self, data: &str) -> Result<Vec<StreamDelta>, String> {
        if data.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let v: serde_json::Value = serde_json::from_str(data)
            .map_err(|e| format!("malformed openai-compatible stream chunk: {e}"))?;
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(format!("provider stream error: {msg}"));
        }
        self.started = true;
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = Usage {
                input_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
            };
        }
        let mut out = Vec::new();
        let Some(choice) = v.pointer("/choices/0") else {
            return Ok(out);
        };
        if let Some(r) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            self.stop_reason = r.to_string();
        }
        let delta = &choice["delta"];
        if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                self.content.push_str(text);
                out.push(StreamDelta::Text(text.to_string()));
            }
        }
        for tc in delta
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .into_iter()
            .flatten()
        {
            let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            if self.tools.len() <= idx {
                self.tools.resize(idx + 1, Default::default());
            }
            let slot = &mut self.tools[idx];
            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                slot.0 = id.to_string();
            }
            if let Some(name) = tc.pointer("/function/name").and_then(|n| n.as_str()) {
                slot.1.push_str(name);
            }
            if let Some(args) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) {
                slot.2.push_str(args);
            }
        }
        Ok(out)
    }

    fn finish(self: Box<Self>) -> Result<ChatResult, String> {
        if !self.started {
            return Err("openai-compatible stream ended without any chunk".into());
        }
        Ok(ChatResult {
            content: self.content,
            tool_calls: self
                .tools
                .into_iter()
                .filter(|(_, name, _)| !name.is_empty())
                .map(|(id, name, arguments_json)| ToolCall {
                    id,
                    name,
                    arguments_json,
                })
                .collect(),
            stop_reason: self.stop_reason,
            usage: self.usage,
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

    fn params(base_url: &str) -> ChatCompletionParams {
        ChatCompletionParams {
            provider: ReqProvider::OpenAi,
            base_url: base_url.to_string(),
            model: "gpt-4o".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
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
    fn builds_request_with_bearer_auth() {
        let req = OpenAiCompatProvider
            .build_http_request(&params("https://api.openai.com/v1"), "sk-secret");
        assert_eq!(req.url, "https://api.openai.com/v1/chat/completions");
        assert_eq!(
            req.headers.get("Authorization").unwrap(),
            "Bearer sk-secret"
        );
        assert_eq!(req.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(req.retry_backoff_ms, DEFAULT_RETRY_BACKOFF_MS);
    }

    #[test]
    fn omits_auth_header_when_key_empty() {
        let req = OpenAiCompatProvider.build_http_request(&params("http://localhost:11434/v1"), "");
        assert!(!req.headers.contains_key("Authorization"));
    }

    #[test]
    fn strips_trailing_slash_from_base_url() {
        let req =
            OpenAiCompatProvider.build_http_request(&params("https://openrouter.ai/api/v1/"), "k");
        assert_eq!(req.url, "https://openrouter.ai/api/v1/chat/completions");
    }

    #[test]
    fn image_messages_become_data_url_parts() {
        let mut p = params("http://x/v1");
        p.messages[0].images.push(ImageBlock {
            mime_type: "image/jpeg".to_string(),
            data_base64: "aGVsbG8=".to_string(),
        });
        let req = OpenAiCompatProvider.build_http_request(&p, "k");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/jpeg;base64,aGVsbG8="
        );
    }

    #[test]
    fn plain_text_messages_keep_string_content() {
        let req = OpenAiCompatProvider.build_http_request(&params("http://x/v1"), "k");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn tools_are_wrapped_as_functions_with_parameters() {
        let mut p = params("http://x/v1");
        p.tools.push(ToolSpec {
            name: "launch".to_string(),
            description: "Launch an app".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        });
        let req = OpenAiCompatProvider.build_http_request(&p, "k");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "launch");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn parses_valid_response() {
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2}
        })
        .to_string();
        let result = OpenAiCompatProvider
            .parse_response(body.as_bytes())
            .unwrap();
        assert_eq!(result.content, "hello");
        assert_eq!(result.stop_reason, "stop");
        assert_eq!(result.usage.input_tokens, 4);
        assert_eq!(result.usage.output_tokens, 2);
        assert!(result.tool_calls.is_empty());
    }

    #[test]
    fn parses_tool_calls_and_null_content() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "launch", "arguments": "{\"app_id\":\"firefox\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
        .to_string();
        let result = OpenAiCompatProvider
            .parse_response(body.as_bytes())
            .unwrap();
        assert_eq!(result.content, "");
        assert_eq!(result.stop_reason, "tool_calls");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "call_1");
        assert_eq!(result.tool_calls[0].name, "launch");
        assert_eq!(
            result.tool_calls[0].arguments_json,
            "{\"app_id\":\"firefox\"}"
        );
    }

    #[test]
    fn parses_mimo_shape_with_explicit_null_tool_calls() {
        // Exact shape opencode returns for mimo-v2.5 on a plain-text reply.
        let body = serde_json::json!({
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "Ок 🙂",
                    "reasoning_content": "thinking...",
                    "tool_calls": null
                }
            }],
            "usage": {"prompt_tokens": 251, "completion_tokens": 65}
        })
        .to_string();
        let result = OpenAiCompatProvider
            .parse_response(body.as_bytes())
            .unwrap();
        assert_eq!(result.content, "Ок 🙂");
        assert!(result.tool_calls.is_empty());
    }

    #[test]
    fn rejects_response_with_no_choices() {
        let body = serde_json::json!({"choices": []}).to_string();
        let err = OpenAiCompatProvider
            .parse_response(body.as_bytes())
            .unwrap_err();
        assert!(err.contains("no choices"), "error was: {err}");
    }

    #[test]
    fn rejects_malformed_json() {
        let err = OpenAiCompatProvider
            .parse_response(b"not json")
            .unwrap_err();
        assert!(err.contains("malformed"), "error was: {err}");
    }

    #[test]
    fn stream_parser_rebuilds_text_tools_and_usage() {
        let mut p = OpenAiCompatProvider.stream_parser();
        let mut streamed = String::new();
        for e in [
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"При"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"вет"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lights","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"on\":"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"true}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":null}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":5}}"#,
            "[DONE]",
        ] {
            for d in p.feed(e).unwrap() {
                let StreamDelta::Text(t) = d;
                streamed.push_str(&t);
            }
        }
        let r = p.finish().unwrap();
        assert_eq!(r.content, "Привет");
        assert_eq!(streamed, r.content);
        assert_eq!(
            r.tool_calls,
            vec![ToolCall {
                id: "call_1".into(),
                name: "lights".into(),
                arguments_json: r#"{"on":true}"#.into(),
            }]
        );
        assert_eq!(r.stop_reason, "tool_calls");
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 7,
                output_tokens: 5
            }
        );
    }

    #[test]
    fn stream_error_chunk_fails_and_request_asks_for_usage() {
        let mut p = OpenAiCompatProvider.stream_parser();
        assert!(p
            .feed(r#"{"error":{"message":"rate limited"}}"#)
            .unwrap_err()
            .contains("rate limited"));
        let req =
            OpenAiCompatProvider.build_stream_request(&params("https://api.openai.com/v1"), "k");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }
}
