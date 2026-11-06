//! Request parsing for the `agent` plugin: turn raw `params_json` into
//! typed request values at the boundary, so the interior never re-validates.
//! Errors name the offending field (house style).

pub const GOAL_MAX: usize = 4000;
pub const CONTEXT_MAX: usize = 8000;
pub const TITLE_MAX: usize = 200;
/// Hard cap on goal-loop iterations; keeps a runaway model bounded.
pub const MAX_STEPS_CAP: u32 = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct LlmOverrides {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub agent_id: Option<String>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GoalStartParams {
    pub goal: String,
    pub context: String,
    pub title: Option<String>,
    pub max_steps: u32,
    pub llm: LlmOverrides,
    /// Existing chat session to continue (`chats` id).
    pub session_id: Option<String>,
    /// `chat` | `voice`; `None` = default (`voice`).
    pub kind: Option<String>,
    /// Free-form origin label, e.g. `desktop`.
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentRequest {
    GoalStart(GoalStartParams),
    GoalGet { id: String },
    GoalList { limit: usize },
    GoalResume { id: String, approve: bool },
    ToolsList,
    /// `memory_forget {"query": "…"}` — delete the top similar fact.
    MemoryForget { query: String },
    /// `memory_clear {}` — wipe every stored fact.
    MemoryClear,
    /// `memory_list {}` — index contents, newest first.
    MemoryList,
}

fn want_string(body: &serde_json::Value, field: &str) -> Result<String, String> {
    body.get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required field: {field}"))
}

fn check_size(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        Err(format!("params.{field} exceeds {max} bytes (got {})", value.len()))
    } else {
        Ok(())
    }
}

/// `api_key_env` is an env-var-style lookup handle, never a literal key:
/// reject anything with whitespace so it can't smuggle prose/keys inline.
fn check_key_env(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.split_whitespace().count() != 1 {
        Err(format!(
            "params.{field} must be a single env-var-style name (no whitespace)"
        ))
    } else {
        Ok(())
    }
}

fn check_base_url(value: &str) -> Result<(), String> {
    if value.starts_with("http://") || value.starts_with("https://") {
        Ok(())
    } else {
        Err("params.base_url must start with http:// or https://".to_string())
    }
}

fn parse_goal_start(body: &serde_json::Value) -> Result<GoalStartParams, String> {
    let goal = want_string(body, "goal")?;
    if goal.trim().is_empty() {
        return Err("params.goal must be a non-empty string".to_string());
    }
    check_size("goal", &goal, GOAL_MAX)?;

    let context = body
        .get("context")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    check_size("context", &context, CONTEXT_MAX)?;

    let title = match body.get("title").and_then(serde_json::Value::as_str) {
        Some(t) => {
            check_size("title", t, TITLE_MAX)?;
            Some(t.to_string())
        }
        None => None,
    };

    let max_steps = match body.get("max_steps") {
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| "params.max_steps must be an integer".to_string())?;
            u32::try_from(n).map_err(|_| "params.max_steps out of range".to_string())?
        }
        None => 0, // 0 = use the operator default (env), resolved by the engine
    };
    if max_steps > MAX_STEPS_CAP {
        return Err(format!("params.max_steps exceeds {MAX_STEPS_CAP}"));
    }

    let opt_str = |field: &str| -> Result<Option<String>, String> {
        match body.get(field).and_then(serde_json::Value::as_str) {
            Some("") => Ok(None),
            Some(s) => Ok(Some(s.to_string())),
            None => Ok(None),
        }
    };

    let provider = opt_str("provider")?;
    if let Some(p) = &provider {
        if p != "anthropic" && p != "openai" {
            return Err(format!("params.provider must be \"anthropic\" or \"openai\" (got \"{p}\")"));
        }
    }
    let base_url = opt_str("base_url")?;
    if let Some(u) = &base_url {
        check_base_url(u)?;
    }
    let model = opt_str("model")?;
    if let Some(m) = &model {
        check_size("model", m, 200)?;
    }
    let api_key_env = opt_str("api_key_env")?;
    if let Some(k) = &api_key_env {
        check_key_env("api_key_env", k)?;
    }
    let agent_id = opt_str("agent_id")?;
    if let Some(a) = &agent_id {
        check_size("agent_id", a, 100)?;
    }

    let max_tokens = body
        .get("max_tokens")
        .and_then(serde_json::Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX));

    let session_id = opt_str("session_id")?;
    if let Some(sid) = &session_id {
        check_size("session_id", sid, 64)?;
        if !sid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err("params.session_id may contain only [0-9A-Za-z_-]".to_string());
        }
    }
    let kind = opt_str("kind")?;
    if let Some(k) = &kind {
        if k != "chat" && k != "voice" {
            return Err(format!("params.kind must be \"chat\" or \"voice\" (got \"{k}\")"));
        }
    }
    let source = opt_str("source")?;
    if let Some(s) = &source {
        check_size("source", s, 64)?;
    }

    Ok(GoalStartParams {
        goal,
        context,
        title,
        max_steps,
        llm: LlmOverrides { provider, base_url, model, api_key_env, agent_id, max_tokens },
        session_id,
        kind,
        source,
    })
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<AgentRequest, String> {
    let body: serde_json::Value = serde_json::from_slice(params_json)
        .map_err(|e| format!("invalid JSON params for {action}: {e}"))?;
    match action {
        "goal_start" => Ok(AgentRequest::GoalStart(parse_goal_start(&body)?)),
        "goal_get" => Ok(AgentRequest::GoalGet { id: want_string(&body, "id")? }),
        "goal_list" => {
            let limit = body.get("limit").and_then(serde_json::Value::as_u64).unwrap_or(50);
            let limit = usize::try_from(limit)
                .unwrap_or(50)
                .clamp(1, 200);
            Ok(AgentRequest::GoalList { limit })
        }
        "goal_resume" => {
            let id = want_string(&body, "id")?;
            let approve = body
                .get("approve")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| "missing required field: approve (boolean)".to_string())?;
            Ok(AgentRequest::GoalResume { id, approve })
        }
        "tools_list" => Ok(AgentRequest::ToolsList),
        "memory_forget" => {
            let query = want_string(&body, "query")?;
            check_size("query", &query, 2000)?;
            Ok(AgentRequest::MemoryForget { query })
        }
        "memory_clear" => Ok(AgentRequest::MemoryClear),
        "memory_list" => Ok(AgentRequest::MemoryList),
        other => Err(format!("ERR_AGENT_BAD_PARAMS: unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(action: &str, v: serde_json::Value) -> Result<AgentRequest, String> {
        parse_request(action, serde_json::to_vec(&v).unwrap().as_slice())
    }

    #[test]
    fn parses_minimal_goal_start() {
        match parse("goal_start", json!({"goal": "clean the inbox"})).unwrap() {
            AgentRequest::GoalStart(p) => {
                assert_eq!(p.goal, "clean the inbox");
                assert_eq!(p.context, "");
                assert_eq!(p.title, None);
                assert_eq!(p.max_steps, 0);
                assert_eq!(p.llm, LlmOverrides {
                    provider: None, base_url: None, model: None,
                    api_key_env: None, agent_id: None, max_tokens: None,
                });
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_full_goal_start_with_llm_overrides() {
        let req = parse(
            "goal_start",
            json!({
                "goal": "g", "context": "c", "title": "t",
                "max_steps": 8,
                "provider": "anthropic",
                "base_url": "https://api.test",
                "model": "claude-x",
                "api_key_env": "MY_KEY",
                "max_tokens": 512
            }),
        )
        .unwrap();
        match req {
            AgentRequest::GoalStart(p) => {
                assert_eq!(p.max_steps, 8);
                assert_eq!(p.llm.provider.as_deref(), Some("anthropic"));
                assert_eq!(p.llm.model.as_deref(), Some("claude-x"));
                assert_eq!(p.llm.max_tokens, Some(512));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_empty_and_oversize_goal() {
        let err = parse("goal_start", json!({})).unwrap_err();
        assert!(err.contains("goal"), "{err}");
        let err = parse("goal_start", json!({"goal": "   "})).unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
        let err = parse("goal_start", json!({"goal": "x".repeat(GOAL_MAX + 1)})).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn rejects_bad_max_steps_provider_and_url() {
        let err = parse("goal_start", json!({"goal": "g", "max_steps": 17})).unwrap_err();
        assert!(err.contains("max_steps"), "{err}");
        let err = parse("goal_start", json!({"goal": "g", "provider": "gemini"})).unwrap_err();
        assert!(err.contains("provider"), "{err}");
        let err = parse("goal_start", json!({"goal": "g", "base_url": "ftp://x"})).unwrap_err();
        assert!(err.contains("base_url"), "{err}");
        let err = parse("goal_start", json!({"goal": "g", "api_key_env": "two words"})).unwrap_err();
        assert!(err.contains("api_key_env"), "{err}");
    }

    #[test]
    fn parses_get_list_resume_tools() {
        match parse("goal_get", json!({"id": "7"})).unwrap() {
            AgentRequest::GoalGet { id } => assert_eq!(id, "7"),
            other => panic!("wrong variant: {other:?}"),
        }
        match parse("goal_list", json!({"limit": 500})).unwrap() {
            AgentRequest::GoalList { limit } => assert_eq!(limit, 200, "clamped"),
            other => panic!("wrong variant: {other:?}"),
        }
        match parse("goal_list", json!({})).unwrap() {
            AgentRequest::GoalList { limit } => assert_eq!(limit, 50),
            other => panic!("wrong variant: {other:?}"),
        }
        match parse("goal_resume", json!({"id": "3", "approve": true})).unwrap() {
            AgentRequest::GoalResume { id, approve } => {
                assert_eq!((id.as_str(), approve), ("3", true));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let err = parse("goal_resume", json!({"id": "3"})).unwrap_err();
        assert!(err.contains("approve"), "{err}");
        assert!(matches!(parse("tools_list", json!({})).unwrap(), AgentRequest::ToolsList));
    }

    #[test]
    fn session_fields_default_to_none_and_are_validated() {
        match parse("goal_start", json!({"goal": "g"})).unwrap() {
            AgentRequest::GoalStart(p) => {
                assert_eq!((p.session_id, p.kind, p.source), (None, None, None));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        match parse(
            "goal_start",
            json!({"goal": "g", "session_id": "01J_ab-Z", "kind": "chat", "source": "desktop"}),
        )
        .unwrap()
        {
            AgentRequest::GoalStart(p) => {
                assert_eq!(p.session_id.as_deref(), Some("01J_ab-Z"));
                assert_eq!(p.kind.as_deref(), Some("chat"));
                assert_eq!(p.source.as_deref(), Some("desktop"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let err = parse("goal_start", json!({"goal": "g", "kind": "sms"})).unwrap_err();
        assert!(err.contains("kind"), "{err}");
        let err = parse("goal_start", json!({"goal": "g", "session_id": "x".repeat(65)})).unwrap_err();
        assert!(err.contains("session_id"), "{err}");
        let err = parse("goal_start", json!({"goal": "g", "session_id": "a b"})).unwrap_err();
        assert!(err.contains("session_id"), "{err}");
        let err = parse("goal_start", json!({"goal": "g", "source": "s".repeat(65)})).unwrap_err();
        assert!(err.contains("source"), "{err}");
    }

    #[test]
    fn unknown_action_is_named_error() {
        let err = parse("frobnicate", json!({})).unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }
}
