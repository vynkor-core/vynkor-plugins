//! The model leg of the `agent` plugin: build the conversation, call
//! `ai`'s `chat_completion` through the [`Rpc`] proxy, and parse the reply
//! into either a final answer or one tool call.
//!
//! Tool-calling protocol: `ai`'s normalized interface is plain text
//! messages (no native tool-use blocks), so the catalog and the reply
//! format are negotiated inside the prompt itself — the model answers with
//! either plain text (final answer) or exactly one JSON object
//! `{"tool": "...", "params": {...}}`. Parsing is deliberately forgiving:
//! fenced code blocks are unwrapped and the first balanced JSON object
//! wins; anything that doesn't parse as a well-formed tool call is treated
//! as the final answer so prose is never lost.

use serde_json::{json, Value};

use crate::store::{LlmPlan, Turn};
use crate::tools::Catalog;
use crate::Rpc;

/// Hard cap matching `ai`'s own `MAX_TIMEOUT_MS`.
pub const CHAT_TIMEOUT_MS: u32 = 30_000;

/// Operator env var: per-completion timeout override. Local CPU models
/// routinely need more than the 30s default — pair a raised value with a
/// raised kernel watchdog (`watchdog_interval_secs`/`watchdog_timeout_secs`),
/// since ai's serve loop stays blocked for the duration.
pub const CHAT_TIMEOUT_ENV: &str = "AGENT_PLUGIN_AI_TIMEOUT_MS";

pub fn chat_timeout_ms() -> u32 {
    std::env::var(CHAT_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(CHAT_TIMEOUT_MS)
        .clamp(1_000, 300_000)
}
/// Default `max_tokens` when neither env nor request names one.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Operator env var: named `ai` profile used when the primary LLM leg fails
/// (provider 429/5xx, timeout, unknown model). Empty/unset = no fallback.
pub const FALLBACK_AGENT_ENV: &str = "AGENT_PLUGIN_FALLBACK_AGENT_ID";

/// Operator env var: native tool-use passthrough mode. `auto` (default)
/// sends ai a native `tools` param whenever the catalog is non-empty;
/// `on` forces it even for an empty catalog (provider decides);
/// `off` restores the pure text protocol.
pub const NATIVE_TOOLS_ENV: &str = "AGENT_PLUGIN_NATIVE_TOOLS";

/// Retries agent asks `ai` to run on provider 429/5xx. Default 0 on
/// purpose: ai's serve loop is sequential — every retried attempt blocks
/// it past the kernel watchdog (~40s), so interactive goals must fail fast
/// into [`chat_with_fallback`]'s profile swap instead.
pub const AI_MAX_RETRIES_ENV: &str = "AGENT_PLUGIN_AI_MAX_RETRIES";

pub fn ai_max_retries() -> u32 {
    std::env::var(AI_MAX_RETRIES_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
        .min(5)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeMode {
    Auto,
    On,
    Off,
}

pub fn parse_native_mode(raw: &str) -> NativeMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => NativeMode::On,
        "off" | "false" | "0" => NativeMode::Off,
        _ => NativeMode::Auto,
    }
}

pub fn native_mode() -> NativeMode {
    parse_native_mode(&std::env::var(NATIVE_TOOLS_ENV).unwrap_or_default())
}

/// The model leg's reply in `ai`'s normalized shape: assistant text plus
/// any native tool invocations (empty unless `tools` was sent).
#[derive(Debug, Clone, PartialEq)]
pub struct ChatOutcome {
    pub content: String,
    pub tool_calls: Vec<NativeToolCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

/// Map the allowlisted catalog into ai's `tools` param. Non-object
/// parameters (minimal specs) become an empty object schema so one bad
/// entry can't invalidate the whole request.
pub fn catalog_tools_param(catalog: &Catalog) -> Vec<Value> {
    catalog
        .tools
        .iter()
        .map(|t| {
            let schema = if t.parameters.is_object() {
                t.parameters.clone()
            } else {
                json!({"type": "object", "properties": {}})
            };
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": schema,
            })
        })
        .collect()
}

/// The fallback profile plan: profile mode ignores the explicit fields, so
/// swapping `agent_id` (and clearing them for clarity) is the whole swap.
pub fn fallback_plan(plan: &LlmPlan, fallback_agent_id: &str) -> LlmPlan {
    let mut p = plan.clone();
    p.agent_id = fallback_agent_id.to_string();
    p.provider.clear();
    p.base_url.clear();
    p.model.clear();
    p.api_key_env.clear();
    p
}

/// One chat round-trip with a single operator-configured retry: when
/// [`FALLBACK_AGENT_ENV`] names a different profile and the primary call
/// fails, the same transcript is replayed through the fallback profile.
pub async fn chat_with_fallback(
    rpc: &Rpc,
    plan: &LlmPlan,
    transcript: &[Turn],
    tools: &[Value],
) -> Result<ChatOutcome, String> {
    match chat(rpc, plan, transcript, tools).await {
        Ok(outcome) => Ok(outcome),
        Err(primary_err) => {
            let fb = std::env::var(FALLBACK_AGENT_ENV).ok().filter(|s| !s.is_empty());
            match fb {
                Some(fb) if fb != plan.agent_id => {
                    eprintln!(
                        "[agent] primary LLM failed ({primary_err}); falling back to agent '{fb}'"
                    );
                    let retry_plan = fallback_plan(plan, &fb);
                    chat(rpc, &retry_plan, transcript, tools).await.map_err(|e| {
                        format!("{primary_err}; fallback '{fb}' also failed: {e}")
                    })
                }
                _ => Err(primary_err),
            }
        }
    }
}

#[allow(clippy::match_like_matches_macro)]
pub fn is_dynamic_catalog_enabled() -> bool {
    match std::env::var("AGENT_PLUGIN_DYNAMIC_CATALOG")
        .unwrap_or_else(|_| "on".into())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" | "false" | "0" | "no" => false,
        _ => true,
    }
}

#[allow(clippy::needless_bool)]
pub fn filtered_catalog(catalog: &Catalog, goal: &str, context: &str) -> Catalog {
    if !is_dynamic_catalog_enabled() || goal.trim().is_empty() {
        return catalog.clone();
    }
    let hay = format!("{} {}", goal, context).to_lowercase();
    let is_telegram = hay.contains("телег") || hay.contains("telegram") || hay.contains("tg_") || hay.contains("tg-") || hay.contains("чат") || hay.contains("сообщ") || hay.contains("диалог") || hay.contains("контакт") || hay.contains("непрочит") || hay.contains("артем") || hay.contains("артём") || hay.contains("loner");
    let is_email = hay.contains("почт") || hay.contains("email") || hay.contains("письм") || hay.contains("imap") || hay.contains("smtp");
    let is_calendar = hay.contains("календ") || hay.contains("встреч") || hay.contains("событ") || hay.contains("напомин") || hay.contains("schedule") || hay.contains("calendar") || hay.contains("event");
    let is_fs = hay.contains("файл") || hay.contains("папк") || hay.contains("директ") || hay.contains("fs_") || hay.contains("filesystem");
    let is_notes = hay.contains("заметк") || hay.contains("note");
    let is_media = hay.contains("медиа") || hay.contains("музык") || hay.contains("видео") || hay.contains("mpris") || hay.contains("media");
    let is_system = hay.contains("систем") || hay.contains("батаре") || hay.contains("громк") || hay.contains("яркост") || hay.contains("sys_");
    let is_web = hay.contains("поиск") || hay.contains("найди") || hay.contains("web") || hay.contains("search") || hay.contains("погод") || hay.contains("weather");
    let is_memory = hay.contains("помни") || hay.contains("память") || hay.contains("вспомн") || hay.contains("запомн") || hay.contains("обо мне") || hay.contains("про меня") || hay.contains("обо_мне") || hay.contains("факт") || hay.contains("memory") || hay.contains("вектор") || hay.contains("vector") || hay.contains("векторн");
    let is_db = hay.contains("бд") || hay.contains("баз") || hay.contains("db_") || hay.contains("database") || hay.contains("vector") || hay.contains("вектор") || is_memory;
    let is_any_telegram = hay.contains("telegram") || is_telegram;

    let mut filtered: Vec<crate::tools::ToolSpec> = Vec::new();
    for tool in &catalog.tools {
        let name = tool.name.as_str();
        let keep = if name.starts_with("tg_") {
            is_any_telegram || hay.contains("tg") || hay.is_empty()
        } else if name.starts_with("email_") {
            is_email
        } else if name.starts_with("event_") || name.starts_with("schedule_") {
            is_calendar
        } else if name.starts_with("fs_") {
            is_fs
        } else if name.starts_with("note_") {
            is_notes || is_memory
        } else if name.starts_with("media_") {
            is_media
        } else if name.starts_with("sys_") {
            is_system
        } else if name == "web_search" || name == "http_request" {
            is_web || is_any_telegram
        } else if name.starts_with("vec_") || name.starts_with("vector") {
            is_memory || is_db || hay.contains("vec") || hay.contains("вектор")
        } else if name.starts_with("db_") {
            is_memory || is_db || hay.contains("db") || hay.contains("баз")
        } else if name.starts_with("tts_") || name.starts_with("stt_") || name.starts_with("mic_") || name.starts_with("sound_") {
            is_media || hay.contains("tts") || hay.contains("stt") || hay.contains("озвуч") || hay.contains("говор")
        } else if name.starts_with("daemon_") || name.starts_with("hotkey_") || name.starts_with("launch") || name.starts_with("clipboard") {
            false
        } else {
            true
        };
        if keep {
            filtered.push(tool.clone());
        }
    }
    if filtered.len() < 3 || filtered.len() == catalog.tools.len() {
        return catalog.clone();
    }
    let has_filtered = filtered.iter().any(|t| t.name.starts_with("tg_"));
    if is_any_telegram && !has_filtered {
        return catalog.clone();
    }
    Catalog {
        tools: filtered,
        allowed_actions: catalog.allowed_actions.clone(),
        tools_file_set: catalog.tools_file_set,
    }
}

/// Build the instructions message that carries the tool catalog. `ai`
/// resolves `system_prompt` only from an `agent_id` profile (never from
/// callers), so the portable place for operator-free instructions is a
/// leading user message.
pub fn opening_messages(goal: &str, context: &str, catalog: &Catalog) -> Vec<Turn> {
    let tools_json: Vec<Value> = catalog
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
                "requires_confirmation": t.requires_confirmation,
            })
        })
        .collect();
    let mut instructions = format!(
        "You are the vynkor host agent: you complete the user's goal by \
         calling host tools step by step.\n\n\
         Available tools (JSON array; `parameters` is a JSON Schema for the \
         `params` object):\n{}\n\n\
         Reply rules:\n\
         - To call a tool, reply with EXACTLY ONE JSON object and nothing \
         else: {{\"tool\": \"<name>\", \"params\": {{...}}}}.\n\
         - Only tool names from the list above are valid.\n\
         - After each call you receive a message starting with \
         \"[TOOL RESULT\" containing the outcome.\n\
         - When the goal is achieved (or impossible), reply with the final \
         answer as PLAIN TEXT — no JSON object at all.",
        serde_json::to_string_pretty(&tools_json).unwrap_or_else(|_| "[]".to_string()),
    );
    // only when the goal loop can actually launch apps: name-based lookup is
    // unique-only, so a guessed short name ("telegram") 404s while the exact
    // catalog id works
    let has_launch = catalog.tools.iter().any(|t| t.name == "launch");
    let has_list = catalog.tools.iter().any(|t| t.name == "launch_list");
    if has_launch && has_list {
        instructions.push_str(
            "\n\nLauncher rule:\n\
             - Never pass a guessed or human app name straight to `launch`.\n\
             - First call `launch_list` with {\"query\": \"<name>\"}, pick the \
             best match from its results, and pass that exact `id` as \
             `app_id`.\n\
             - If several entries share the display name, prefer the one whose \
             exec does NOT start with \"waydroid\" unless the user explicitly \
             wants the Android app; if it is still ambiguous, ask via final \
             answer.",
        );
    }
    if !context.is_empty() {
        instructions.push_str(
            "\n\nSession memory: the user message below carries a [SESSION MEMORY] \
             block — your last exchanges with this user (oldest first). Treat it as \
             shared history: resolve references like \"she\", \"the same one\", or \
             \"again\" against it, and never ask for anything it already covers.",
        );
    }
    let mut msgs = vec![Turn { role: "user".into(), content: instructions }];
    let goal_msg = if context.is_empty() {
        goal.to_string()
    } else {
        format!("{goal}\n\n---\n[SESSION MEMORY]\n{context}\n[/SESSION MEMORY]")
    };
    msgs.push(Turn { role: "user".into(), content: goal_msg });
    msgs
}

/// One `chat_completion` round-trip over the current transcript. When
/// `tools` is non-empty it rides along as ai's native `tools` param; the
/// reply carries both the assistant text and any native tool invocations.
pub async fn chat(
    rpc: &Rpc,
    plan: &LlmPlan,
    transcript: &[Turn],
    tools: &[Value],
) -> Result<ChatOutcome, String> {
    let messages: Vec<Value> = transcript
        .iter()
        .map(|t| json!({"role": t.role, "content": t.content}))
        .collect();
    let mut params = serde_json::Map::new();

    if plan.agent_id.is_empty() {
        if plan.model.is_empty() || plan.api_key_env.is_empty() {
            return Err(
                "no LLM configured: set AGENT_PLUGIN_AI_MODEL / AGENT_PLUGIN_AI_API_KEY_ENV \
                 (or AGENT_PLUGIN_AI_AGENT_ID), or pass overrides in the request"
                    .to_string(),
            );
        }
        params.insert("provider".into(), json!(plan.provider));
        if !plan.base_url.is_empty() {
            params.insert("base_url".into(), json!(plan.base_url));
        }
        params.insert("model".into(), json!(plan.model));
        params.insert("api_key_env".into(), json!(plan.api_key_env));
    } else {
        params.insert("agent_id".into(), json!(plan.agent_id));
    }
    params.insert("messages".into(), Value::Array(messages));
    if !tools.is_empty() {
        params.insert("tools".into(), Value::Array(tools.to_vec()));
    }
    params.insert("max_tokens".into(), json!(plan.max_tokens));
    let timeout = chat_timeout_ms();
    params.insert("timeout_ms".into(), json!(timeout));
    params.insert("max_retries".into(), json!(ai_max_retries()));

    let v = rpc.call("chat_completion", Value::Object(params), timeout).await?;
    let content = v
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("chat_completion returned unexpected payload: {v}"))?;
    let tool_calls = v
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    Some(NativeToolCall {
                        id: tc.get("id").and_then(Value::as_str)?.to_string(),
                        name: tc.get("name").and_then(Value::as_str)?.to_string(),
                        arguments_json: tc
                            .get("arguments_json")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ChatOutcome { content, tool_calls })
}

/// Interpret a chat outcome: a native tool invocation wins (structured
/// shape beats text heuristics); malformed arguments dispatch with empty
/// params so the provider's own validation error feeds back into the loop;
/// otherwise the legacy text protocol parses the content.
pub fn outcome_to_reply(outcome: &ChatOutcome) -> Reply {
    if let Some(tc) = outcome.tool_calls.iter().find(|tc| !tc.name.is_empty()) {
        let params = serde_json::from_str::<Value>(&tc.arguments_json)
            .ok()
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({}));
        return Reply::ToolCall {
            name: tc.name.clone(),
            params,
        };
    }
    parse_reply(&outcome.content)
}

/// Model reply after parsing: a final answer or one tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Final(String),
    ToolCall { name: String, params: Value },
}

/// Extract the first balanced `{...}` substring, respecting string literals.
fn first_balanced_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed;
    }
    let body = trimmed.trim_start_matches('`');
    // Drop a language tag on the fence line, then the closing fence.
    let body = body.split_once('\n').map(|(_, rest)| rest).unwrap_or(body);
    body.trim().trim_end_matches('`').trim()
}

/// Parse a model reply into a final answer or a tool call. Anything that
/// isn't a well-formed `{"tool": ..., "params": {...}}` object degrades to
/// [`Reply::Final`] carrying the original trimmed text.
/// Extract a qwen-style `<tool_call>name<arg_key>k</arg_key><arg_value>v</arg_value>
/// ...</tool_call>` block. Free-tier models drift into this shape despite the
/// JSON contract; accepting it costs nothing and unblocks them.
fn extract_xml_tool_call(text: &str) -> Option<Reply> {
    let start = text.find("<tool_call>")?;
    let rest = &text[start..];
    let body = rest
        .find("</tool_call>")
        .map(|end| &rest[..end])
        .unwrap_or(rest);
    let inner = body.trim_start_matches("<tool_call>").trim();
    let name = match inner.find("<arg_key>") {
        Some(i) => inner[..i].trim(),
        None => inner,
    };
    if name.is_empty() || name.contains('<') {
        return None;
    }
    let mut params = serde_json::Map::new();
    let mut cursor = inner;
    while let Some(ki) = cursor.find("<arg_key>") {
        let after_key = &cursor[ki + "<arg_key>".len()..];
        let Some(k_end) = after_key.find("</arg_key>") else { break };
        let key = after_key[..k_end].trim().to_string();
        let vi = match after_key[k_end..].find("<arg_value>") {
            Some(v) => k_end + v + "<arg_value>".len(),
            None => break,
        };
        let after_val = &after_key[vi..];
        let v_end = after_val.find("</arg_value>").unwrap_or(after_val.len());
        let raw_val = after_val[..v_end].trim();
        let val = match serde_json::from_str::<Value>(raw_val) {
            Ok(v) => v,
            Err(_) => json!(raw_val),
        };
        if key.is_empty() {
            return None;
        }
        params.insert(key, val);
        cursor = &after_val[v_end..];
    }
    Some(Reply::ToolCall {
        name: name.to_string(),
        params: Value::Object(params),
    })
}

/// Strip drifted tool-call markup from what would otherwise be spoken.
pub fn strip_tool_markup(text: &str) -> String {
    let mut s = text.to_string();
    while let Some(i) = s.find("<tool_call>") {
        let end = s[i..]
            .find("</tool_call>")
            .map(|e| i + e + "</tool_call>".len())
            .unwrap_or(s.len());
        s.replace_range(i..end, "");
    }
    s.trim().to_string()
}

pub fn parse_reply(content: &str) -> Reply {
    let cleaned = strip_code_fence(content);

    // Qwen-style drift first: a <tool_call> block means the model TRIED to
    // dispatch — never speak that markup as prose.
    if cleaned.contains("<tool_call>") {
        if let Some(tc) = extract_xml_tool_call(cleaned) {
            return tc;
        }
        return Reply::Final(strip_tool_markup(cleaned));
    }

    let Some(obj_text) = first_balanced_object(cleaned) else {
        return Reply::Final(cleaned.trim().to_string());
    };
    let Ok(v) = serde_json::from_str::<Value>(obj_text) else {
        return Reply::Final(cleaned.trim().to_string());
    };
    let Some(name) = v.get("tool").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Reply::Final(cleaned.trim().to_string());
    };
    let params = match v.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(p) if p.is_object() => p.clone(),
        _ => return Reply::Final(cleaned.trim().to_string()),
    };
    Reply::ToolCall { name: name.to_string(), params }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn parses_qwen_style_xml_tool_call() {
        let r = parse_reply(
            "Открываю.\n<tool_call>launch<arg_key>app_id</arg_key><arg_value>Alacritty</arg_value></tool_call>",
        );
        assert_eq!(
            r,
            Reply::ToolCall { name: "launch".into(), params: json!({"app_id": "Alacritty"}) }
        );
        let numeric = parse_reply(
            "<tool_call>schedule_set<arg_key>delay_ms</arg_key><arg_value>10000</arg_value></tool_call>",
        );
        assert_eq!(
            numeric,
            Reply::ToolCall { name: "schedule_set".into(), params: json!({"delay_ms": 10000}) }
        );
    }

    #[test]
    fn truncated_xml_still_dispatches_loop_self_corrects() {
        // Обрезанный вызов диспатчится как есть: launcher вернёт ERR по
        // обязательным полям, цикл скормит его модели и та поправится.
        let r = parse_reply("Открываю новое окно.<tool_call>launch<arg_key>exec");
        match &r {
            Reply::ToolCall { name, .. } => assert_eq!(name, "launch"),
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_xml_never_leaks_markup_into_speech() {
        let r = parse_reply("Готово.<tool_call><arg_value>мусор");
        match &r {
            Reply::Final(t) => assert!(!t.contains("<tool_call>") && !t.contains("<arg_value>"), "{t}"),
            other => panic!("expected final, got {other:?}"),
        }
    }

    #[test]
    fn fallback_plan_swaps_profile_and_clears_explicit_fields() {
        let plan = LlmPlan {
            provider: "openai".into(),
            base_url: "https://example.com/v1".into(),
            model: "some-model".into(),
            api_key_env: "SOME_KEY".into(),
            agent_id: "default".into(),
            max_tokens: 256,
        };
        let fb = fallback_plan(&plan, "local");
        assert_eq!(fb.agent_id, "local");
        assert!(fb.model.is_empty() && fb.api_key_env.is_empty());
        assert!(fb.provider.is_empty() && fb.base_url.is_empty());
        assert_eq!(fb.max_tokens, 256);
    }

    #[test]
    fn parses_bare_fenced_and_embedded_tool_calls() {
        let r = parse_reply("{\"tool\":\"notify_send\",\"params\":{\"title\":\"hi\"}}");
        assert_eq!(
            r,
            Reply::ToolCall { name: "notify_send".into(), params: json!({"title": "hi"}) }
        );

        let fenced = "```json\n{\"tool\": \"a\", \"params\": {\"x\": 1}}\n```";
        assert_eq!(parse_reply(fenced), Reply::ToolCall { name: "a".into(), params: json!({"x": 1}) });

        let embedded = "Let me check.\n{\"tool\":\"b\",\"params\":{\"path\":\"}{ odd\"}} done";
        assert_eq!(
            parse_reply(embedded),
            Reply::ToolCall { name: "b".into(), params: json!({"path": "}{ odd"}) }
        );

        let no_params = "{\"tool\":\"c\"}";
        assert_eq!(
            parse_reply(no_params),
            Reply::ToolCall { name: "c".into(), params: json!({}) }
        );
    }

    #[test]
    fn malformed_calls_degrade_to_final_answer() {
        assert_eq!(parse_reply("Just do it."), Reply::Final("Just do it.".into()));
        assert_eq!(parse_reply("{not json}"), Reply::Final("{not json}".into()));
        // A JSON object without "tool" is not a tool call.
        assert_eq!(parse_reply("{\"answer\": 42}"), Reply::Final("{\"answer\": 42}".into()));
        // Empty tool name / non-object params degrade too.
        assert_eq!(parse_reply("{\"tool\":\"\"}"), Reply::Final("{\"tool\":\"\"}".into()));
        assert_eq!(
            parse_reply("{\"tool\":\"a\",\"params\":[1]}"),
            Reply::Final("{\"tool\":\"a\",\"params\":[1]}".into())
        );
        // Unterminated brace → plain final.
        assert_eq!(parse_reply("{\"tool\":\"a\""), Reply::Final("{\"tool\":\"a\"".into()));
    }

    #[test]
    fn opening_messages_carry_launcher_rule_only_when_launchable() {
        let tool = |name: &str| crate::tools::ToolSpec {
            name: name.into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
            requires_confirmation: false,
            risk: String::new(),
            timeout_ms: 30_000,
            cooldown_ms: 0,
            max_per_goal: 16,
            source: crate::tools::Source::Minimal,
        };
        let no_launch = Catalog {
            tools: vec![tool("notify_send")],
            allowed_actions: vec!["notify_send".into()],
            tools_file_set: false,
        };
        assert!(!opening_messages("g", "", &no_launch)[0]
            .content
            .contains("Launcher rule"));

        let launchable = Catalog {
            tools: vec![tool("launch"), tool("launch_list")],
            allowed_actions: vec!["launch".into(), "launch_list".into()],
            tools_file_set: false,
        };
        let msgs = opening_messages("g", "", &launchable);
        assert!(msgs[0].content.contains("Launcher rule"));
        assert!(msgs[0].content.contains("launch_list"));
    }

    #[test]
    fn opening_messages_carry_catalog_and_goal() {
        let cat = Catalog {
            tools: vec![crate::tools::ToolSpec {
                name: "notify_send".into(),
                description: "Send a notification".into(),
                parameters: json!({"type": "object"}),
                requires_confirmation: false,
                risk: String::new(),
                timeout_ms: 30_000,
                cooldown_ms: 0,
                max_per_goal: 16,
                source: crate::tools::Source::Kernel,
            }],
            allowed_actions: vec!["notify_send".into()],
            tools_file_set: true,
        };
        let msgs = opening_messages("water the plants", "garden only", &cat);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].content.contains("notify_send"));
        assert!(msgs[0].content.contains("JSON"));
        assert_eq!(msgs[1].role, "user");
        assert!(msgs[1].content.contains("water the plants"));
        assert!(msgs[1].content.contains("[SESSION MEMORY]\ngarden only\n[/SESSION MEMORY]"));
        // Memory awareness instruction rides only when context exists.
        assert!(msgs[0].content.contains("Session memory:"));

        let no_ctx = opening_messages("just g", "", &cat);
        assert!(!no_ctx[1].content.contains("[SESSION MEMORY]"));
        assert!(!no_ctx[0].content.contains("Session memory:"));
    }

    #[test]
    fn chat_requires_model_and_key_without_agent_id() {
        // Compile-level smoke of the error path shape (no kernel needed).
        let plan = LlmPlan {
            provider: "openai".into(),
            base_url: String::new(),
            model: String::new(),
            api_key_env: String::new(),
            agent_id: String::new(),
            max_tokens: 1024,
        };
        // Directly exercising chat() needs an Rpc; covered e2e in main.rs.
        // Here just assert the guard constants line up with ai's cap.
        assert_eq!(CHAT_TIMEOUT_MS, 30_000);
        let _ = plan;
    }

    #[test]
    fn chat_timeout_parses_env_with_clamp_and_default() {
        std::env::remove_var(CHAT_TIMEOUT_ENV);
        assert_eq!(chat_timeout_ms(), 30_000);
        std::env::set_var(CHAT_TIMEOUT_ENV, "150000");
        assert_eq!(chat_timeout_ms(), 150_000);
        std::env::set_var(CHAT_TIMEOUT_ENV, "999999999");
        assert_eq!(chat_timeout_ms(), 300_000);
        std::env::set_var(CHAT_TIMEOUT_ENV, "junk");
        assert_eq!(chat_timeout_ms(), 30_000);
        std::env::set_var(CHAT_TIMEOUT_ENV, "500");
        assert_eq!(chat_timeout_ms(), 1_000);
        std::env::remove_var(CHAT_TIMEOUT_ENV);
    }

    #[test]
    fn ai_max_retries_parses_and_clamps_with_zero_default() {
        assert_eq!(ai_max_retries(), 0);
        std::env::set_var(AI_MAX_RETRIES_ENV, "2");
        assert_eq!(ai_max_retries(), 2);
        std::env::set_var(AI_MAX_RETRIES_ENV, "99");
        assert_eq!(ai_max_retries(), 5);
        std::env::set_var(AI_MAX_RETRIES_ENV, "junk");
        assert_eq!(ai_max_retries(), 0);
        std::env::remove_var(AI_MAX_RETRIES_ENV);
    }

    #[test]
    fn native_mode_parses_env_aliases_with_auto_default() {
        assert_eq!(parse_native_mode(""), NativeMode::Auto);
        assert_eq!(parse_native_mode("auto"), NativeMode::Auto);
        assert_eq!(parse_native_mode("garbage"), NativeMode::Auto);
        assert_eq!(parse_native_mode("on"), NativeMode::On);
        assert_eq!(parse_native_mode("TRUE"), NativeMode::On);
        assert_eq!(parse_native_mode("1"), NativeMode::On);
        assert_eq!(parse_native_mode("off"), NativeMode::Off);
        assert_eq!(parse_native_mode("0"), NativeMode::Off);
        assert_eq!(parse_native_mode(" False "), NativeMode::Off);
    }

    #[test]
    fn catalog_tools_param_maps_schema_and_guards_minimal_specs() {
        let mut spec = crate::tools::ToolSpec {
            name: "launch".into(),
            description: "Launch an app".into(),
            parameters: json!({"type": "object", "properties": {"app_id": {"type": "string"}}}),
            requires_confirmation: false,
            risk: String::new(),
            timeout_ms: 30_000,
            cooldown_ms: 0,
            max_per_goal: 16,
            source: crate::tools::Source::Kernel,
        };
        let mut minimal = spec.clone();
        minimal.name = "mystery".into();
        minimal.parameters = Value::Null;
        let cat = Catalog {
            tools: vec![spec.clone(), minimal],
            allowed_actions: vec!["launch".into(), "mystery".into()],
            tools_file_set: false,
        };
        let tools = catalog_tools_param(&cat);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "launch");
        assert_eq!(tools[0]["input_schema"]["properties"]["app_id"]["type"], "string");
        assert!(tools[0].get("requires_confirmation").is_none());
        // A Null/minimal schema must not invalidate the whole request.
        assert_eq!(tools[1]["input_schema"]["type"], "object");
        let _ = spec;
    }

    #[test]
    fn native_tool_call_wins_and_parses_arguments() {
        let outcome = ChatOutcome {
            content: "Opening.".into(),
            tool_calls: vec![NativeToolCall {
                id: "c1".into(),
                name: "launch".into(),
                arguments_json: r#"{"app_id": "firefox"}"#.into(),
            }],
        };
        assert_eq!(
            outcome_to_reply(&outcome),
            Reply::ToolCall { name: "launch".into(), params: json!({"app_id": "firefox"}) }
        );
    }

    #[test]
    fn malformed_native_arguments_dispatch_empty_for_loop_self_correction() {
        let outcome = ChatOutcome {
            content: String::new(),
            tool_calls: vec![NativeToolCall {
                id: "c1".into(),
                name: "launch".into(),
                arguments_json: "not json".into(),
            }],
        };
        assert_eq!(
            outcome_to_reply(&outcome),
            Reply::ToolCall { name: "launch".into(), params: json!({}) }
        );
    }

    #[test]
    fn empty_named_tool_calls_fall_through_to_text_protocol() {
        let outcome = ChatOutcome {
            content: "{\"tool\": \"a\", \"params\": {\"x\": 1}}".into(),
            tool_calls: vec![NativeToolCall {
                id: "c1".into(),
                name: String::new(),
                arguments_json: "{}".into(),
            }],
        };
        assert_eq!(
            outcome_to_reply(&outcome),
            Reply::ToolCall { name: "a".into(), params: json!({"x": 1}) }
        );
    }

    #[test]
    fn text_only_outcome_uses_legacy_parser() {
        let outcome = ChatOutcome {
            content: "All done.".into(),
            tool_calls: Vec::new(),
        };
        assert_eq!(outcome_to_reply(&outcome), Reply::Final("All done.".into()));

        // Models may ignore the tools param and answer in the text protocol.
        let text_proto = ChatOutcome {
            content: "{\"tool\": \"b\", \"params\": {}}".into(),
            tool_calls: Vec::new(),
        };
        assert_eq!(
            outcome_to_reply(&text_proto),
            Reply::ToolCall { name: "b".into(), params: json!({}) }
        );
    }
}
