//! Glue: validate a `chat_completion` request, resolve the model/agent from
//! the database, dispatch to the right provider adapter, send the resulting
//! HTTP request through `network`'s `http_request` action, record usage, and
//! map the response back to `ai`'s normalized shape. Also hosts the
//! discovery/listing actions backed by the database.

use crate::config::DiscoverySource;
use crate::db::{AiDb, UsageRow};
use crate::discovery;
use crate::outbound::ActionCaller;
use crate::provider::{
    anthropic::AnthropicProvider, openai_compat::OpenAiCompatProvider, EmbeddingProvider, Provider,
};
use crate::request::{self, ChatCompletionParams, EmbeddingParams, Provider as RequestProvider};

/// `network`'s `http_request` response shape (see
/// `plugins/network/src/handler.rs::HttpResponseJson`) — only the fields
/// `ai` needs to decode.
#[derive(serde::Deserialize)]
struct NetworkHttpResponse {
    status: u16,
    body: String,
    body_encoding: String,
}

/// Handle one `chat_completion` action end to end. `client` is the same
/// connection `ai` used to register with the kernel — see `main.rs` for why
/// a second connection isn't an option. Returns the JSON to place in
/// `ActionResponse.data_json` on success, or a human-readable error
/// (never containing the resolved API key) on failure.
pub async fn handle_chat_completion(
    caller: &mut impl ActionCaller,
    params_json: &[u8],
    db: &AiDb,
) -> Result<Vec<u8>, String> {
    let mut params = request::parse_request(params_json)?;
    resolve_params(&mut params, db)?;

    let allowed_key_envs = request::parse_allowed_key_envs(
        &std::env::var(request::ALLOWED_KEY_ENVS_ENV).unwrap_or_default(),
    );
    if !request::is_allowed_key_env(&params.api_key_env, &allowed_key_envs) {
        return Err(format!(
            "api_key_env '{}' is not in the operator's {} allowlist",
            params.api_key_env,
            request::ALLOWED_KEY_ENVS_ENV
        ));
    }

    let api_key =
        crate::key_resolve::resolve_api_key(caller, &params.api_key_env).await?;

    let provider: &dyn Provider = match params.provider {
        RequestProvider::Anthropic => &AnthropicProvider,
        RequestProvider::OpenAi => &OpenAiCompatProvider,
    };

    let http_req = provider.build_http_request(&params, &api_key);
    let http_req_json = serde_json::to_vec(&http_req)
        .map_err(|e| format!("failed to encode outbound http request: {e}"))?;

    let action_resp = caller
        .call_action("http_request", &http_req_json, params.timeout_ms as u32)
        .await
        .map_err(|e| format!("network plugin call failed: {e}"))?;

    if action_resp.status != vynkor_sdk::proto::ActionStatus::ActionOk as i32 {
        return Err(format!("network plugin error: {}", action_resp.error));
    }

    let net_resp: NetworkHttpResponse = serde_json::from_slice(&action_resp.data_json)
        .map_err(|e| format!("malformed network response: {e}"))?;

    if !(200..300).contains(&net_resp.status) {
        return Err(format!(
            "provider returned HTTP {}: {}",
            net_resp.status, net_resp.body
        ));
    }

    let body_bytes: Vec<u8> = match net_resp.body_encoding.as_str() {
        "base64" => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(&net_resp.body)
                .map_err(|e| format!("malformed base64 response body: {e}"))?
        }
        _ => net_resp.body.into_bytes(),
    };

    let result = provider.parse_response(&body_bytes)?;

    let model_id = params.model.clone();
    if let Err(e) = db.record_usage(&UsageRow {
        agent_id: params.agent_id.clone().unwrap_or_default(),
        model_id: model_id.clone(),
        input_tokens: result.usage.input_tokens,
        output_tokens: result.usage.output_tokens,
    }) {
        eprintln!("[ai] failed to record usage: {e}");
    }
    let _ = db.touch_model(&model_id);

    serde_json::to_vec(&result).map_err(|e| format!("failed to encode response: {e}"))
}

pub async fn handle_embedding(
    caller: &mut impl ActionCaller,
    params_json: &[u8],
    db: &AiDb,
) -> Result<Vec<u8>, String> {
    let mut params = request::parse_embedding_request(params_json)?;
    resolve_embedding_params(&mut params, db)?;

    let allowed_key_envs = request::parse_allowed_key_envs(
        &std::env::var(request::ALLOWED_KEY_ENVS_ENV).unwrap_or_default(),
    );
    if !request::is_allowed_key_env(&params.api_key_env, &allowed_key_envs) {
        return Err(format!(
            "api_key_env '{}' is not in the operator's {} allowlist",
            params.api_key_env,
            request::ALLOWED_KEY_ENVS_ENV
        ));
    }

    let api_key = crate::key_resolve::resolve_api_key(caller, &params.api_key_env).await?;

    let provider: &dyn EmbeddingProvider = &OpenAiCompatProvider;

    let http_req = provider.build_embedding_request(&params, &api_key);
    let http_req_json = serde_json::to_vec(&http_req)
        .map_err(|e| format!("failed to encode outbound http request: {e}"))?;

    let action_resp = caller
        .call_action("http_request", &http_req_json, params.timeout_ms as u32)
        .await
        .map_err(|e| format!("network plugin call failed: {e}"))?;

    if action_resp.status != vynkor_sdk::proto::ActionStatus::ActionOk as i32 {
        return Err(format!("network plugin error: {}", action_resp.error));
    }

    let net_resp: NetworkHttpResponse = serde_json::from_slice(&action_resp.data_json)
        .map_err(|e| format!("malformed network response: {e}"))?;

    if !(200..300).contains(&net_resp.status) {
        return Err(format!(
            "provider returned HTTP {}: {}",
            net_resp.status, net_resp.body
        ));
    }

    let body_bytes: Vec<u8> = match net_resp.body_encoding.as_str() {
        "base64" => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(&net_resp.body)
                .map_err(|e| format!("malformed base64 response body: {e}"))?
        }
        _ => net_resp.body.into_bytes(),
    };

    let result = provider.parse_embedding_response(&body_bytes)?;

    let model_id = params.model.clone();
    if let Err(e) = db.record_usage(&UsageRow {
        agent_id: params.agent_id.clone().unwrap_or_default(),
        model_id: model_id.clone(),
        input_tokens: result.usage.input_tokens,
        output_tokens: result.usage.output_tokens,
    }) {
        eprintln!("[ai] failed to record usage: {e}");
    }
    let _ = db.touch_model(&model_id);

    serde_json::to_vec(&result).map_err(|e| format!("failed to encode response: {e}"))
}

fn resolve_embedding_params(params: &mut EmbeddingParams, db: &AiDb) -> Result<(), String> {
    if let Some(aid) = params.agent_id.clone() {
        let agent = db
            .get_agent(&aid)
            .map_err(|e| format!("agent lookup failed: {e}"))?
            .ok_or_else(|| format!("unknown agent: {aid}"))?;
        let m = db
            .get_model(&agent.model_id)
            .map_err(|e| format!("model lookup failed: {e}"))?
            .ok_or_else(|| format!("agent '{}' references unknown model '{}'", agent.id, agent.model_id))?;
        params.model = m.id.clone();
        params.base_url = m.base_url.clone();
        params.api_key_env = m.api_key_env.clone();
        params.provider = match m.provider.as_str() {
            "openai" => RequestProvider::OpenAi,
            other => return Err(format!("unsupported provider: {other}")),
        };
        return Ok(());
    }
    if !params.model.is_empty() {
        if let Some(m) = db.get_model(&params.model).map_err(|e| format!("model lookup failed: {e}"))? {
            params.provider = match m.provider.as_str() {
                "openai" => RequestProvider::OpenAi,
                other => return Err(format!("unsupported provider: {other}")),
            };
            params.base_url = m.base_url.clone();
            params.api_key_env = m.api_key_env.clone();
            params.model = m.id.clone();
            return Ok(());
        }
    }
    if params.model.is_empty() {
        return Err("missing required field: model".to_string());
    }
    if params.base_url.is_empty() {
        return Err("missing required field: base_url".to_string());
    }
    if params.api_key_env.is_empty() {
        return Err("missing required field: api_key_env".to_string());
    }
    Ok(())
}

/// Resolve the effective model/endpoint for a request: an `agent_id` (or a
/// bare `model` id) is looked up in the database, which carries the
/// provider/base_url/api_key_env that the operator configured or discovery
/// found. Unknown models with fully explicit legacy fields still work.
fn resolve_params(params: &mut ChatCompletionParams, db: &AiDb) -> Result<(), String> {
    let agent = match &params.agent_id {
        Some(aid) => Some(
            db.get_agent(aid)
                .map_err(|e| format!("agent lookup failed: {e}"))?
                .ok_or_else(|| format!("unknown agent: {aid}"))?,
        ),
        None => None,
    };

    let model_id = agent
        .as_ref()
        .map(|a| a.model_id.clone())
        .unwrap_or_else(|| params.model.clone());

    let stored = db
        .get_model(&model_id)
        .map_err(|e| format!("model lookup failed: {e}"))?;
    match stored {
        Some(m) => {
            params.model = m.id.clone();
            params.provider = match m.provider.as_str() {
                "anthropic" => RequestProvider::Anthropic,
                "openai" => RequestProvider::OpenAi,
                other => return Err(format!("unsupported provider: {other}")),
            };
            params.base_url =
                if m.base_url.is_empty() && params.provider == RequestProvider::Anthropic {
                    request::DEFAULT_ANTHROPIC_BASE_URL.to_string()
                } else {
                    m.base_url.clone()
                };
            params.api_key_env = m.api_key_env.clone();
        }
        None => {
            if let Some(a) = &agent {
                return Err(format!(
                    "agent '{}' references unknown model '{model_id}'",
                    a.id
                ));
            }
            if params.model.is_empty() {
                return Err("missing required field: model".to_string());
            }
            if params.base_url.is_empty() && params.provider == RequestProvider::OpenAi {
                return Err("missing required field: base_url".to_string());
            }
            if params.api_key_env.is_empty() {
                return Err("missing required field: api_key_env".to_string());
            }
        }
    }

    if let Some(a) = agent {
        params.system_prompt = if a.system_prompt.is_empty() {
            None
        } else {
            Some(a.system_prompt.clone())
        };
    }
    Ok(())
}

/// `list_models` — every model the plugin can complete with, config-declared
/// or discovered, with the `is_default` flag the client uses to preselect.
pub fn handle_list_models(db: &AiDb) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&db.list_models().map_err(|e| format!("db error: {e}"))?)
        .map_err(|e| format!("failed to encode response: {e}"))
}

/// `list_agents` — named agent profiles (model + framing) the client offers
/// as conversation personas.
pub fn handle_list_agents(db: &AiDb) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&db.list_agents().map_err(|e| format!("db error: {e}"))?)
        .map_err(|e| format!("failed to encode response: {e}"))
}

/// `usage_stats` — token usage analytics aggregated over all recorded calls.
pub fn handle_usage_stats(db: &AiDb) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&db.usage_stats().map_err(|e| format!("db error: {e}"))?)
        .map_err(|e| format!("failed to encode response: {e}"))
}

/// `refresh_models` — pull the configured providers' model lists and upsert
/// them into the database.
pub async fn handle_refresh_models(
    caller: &mut impl ActionCaller,
    db: &AiDb,
    sources: &[DiscoverySource],
) -> Result<Vec<u8>, String> {
    let result = discovery::refresh_models(caller, db, sources).await?;
    serde_json::to_vec(&result).map_err(|e| format!("failed to encode response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Agent, Model};

    fn db_with_fixtures() -> AiDb {
        let db = AiDb::open(None).unwrap();
        db.upsert_model(&Model {
            id: "llama3.2".into(),
            provider: "openai".into(),
            base_url: "http://localhost:11434/v1".into(),
            api_key_env: "OLLAMA_API_KEY".into(),
            is_default: true,
            discovered_at: None,
            last_seen: 0,
        })
        .unwrap();
        db.upsert_model(&Model {
            id: "claude-sonnet-5".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            is_default: false,
            discovered_at: None,
            last_seen: 0,
        })
        .unwrap();
        db.upsert_agent(&Agent {
            id: "code".into(),
            name: "Coder".into(),
            model_id: "llama3.2".into(),
            system_prompt: "write only code".into(),
            goal: String::new(),
            description: String::new(),
            is_default: true,
            created_at: 0,
        })
        .unwrap();
        db
    }

    fn params(
        agent_id: Option<&str>,
        model: &str,
        provider: &str,
        base: &str,
        env: &str,
    ) -> ChatCompletionParams {
        ChatCompletionParams {
            provider: match provider {
                "anthropic" => RequestProvider::Anthropic,
                _ => RequestProvider::OpenAi,
            },
            base_url: base.into(),
            model: model.into(),
            api_key_env: env.into(),
            messages: vec![request::Message {
                role: "user".into(),
                content: "hi".into(),
                images: Vec::new(),
            }],
            max_tokens: 128,
            timeout_ms: 1000,
            tools: Vec::new(),
            max_retries: request::DEFAULT_MAX_RETRIES,
            retry_backoff_ms: request::DEFAULT_RETRY_BACKOFF_MS,
            agent_id: agent_id.map(str::to_string),
            system_prompt: None,
        }
    }

    #[test]
    fn resolves_agent_from_db() {
        let db = db_with_fixtures();
        let mut p = params(Some("code"), "", "openai", "", "");
        resolve_params(&mut p, &db).unwrap();
        assert_eq!(p.model, "llama3.2");
        assert_eq!(p.base_url, "http://localhost:11434/v1");
        assert_eq!(p.api_key_env, "OLLAMA_API_KEY");
        assert_eq!(p.system_prompt.as_deref(), Some("write only code"));
    }

    #[test]
    fn resolves_model_by_id_from_db() {
        let db = db_with_fixtures();
        let mut p = params(None, "claude-sonnet-5", "", "", "");
        resolve_params(&mut p, &db).unwrap();
        assert_eq!(p.provider, RequestProvider::Anthropic);
        assert_eq!(p.base_url, "https://api.anthropic.com");
        assert_eq!(p.api_key_env, "ANTHROPIC_API_KEY");
        assert!(p.system_prompt.is_none());
    }

    #[test]
    fn unknown_agent_is_error() {
        let db = db_with_fixtures();
        let mut p = params(Some("nope"), "", "openai", "", "");
        let err = resolve_params(&mut p, &db).unwrap_err();
        assert!(err.contains("unknown agent"), "error was: {err}");
    }

    #[test]
    fn agent_with_unknown_model_is_error() {
        let db = db_with_fixtures();
        let mut p = params(Some("code"), "", "openai", "", "");
        db.delete_model("llama3.2").unwrap();
        let err = resolve_params(&mut p, &db).unwrap_err();
        assert!(err.contains("unknown model"), "error was: {err}");
    }

    #[test]
    fn legacy_call_without_db_model_still_works() {
        let db = db_with_fixtures();
        let mut p = params(None, "my-custom", "openai", "http://x/v1", "OPENAI_API_KEY");
        resolve_params(&mut p, &db).unwrap();
        assert_eq!(p.model, "my-custom");
        assert_eq!(p.base_url, "http://x/v1");
    }

    #[test]
    fn legacy_openai_without_base_url_is_error() {
        let db = db_with_fixtures();
        let mut p = params(None, "my-custom", "openai", "", "OPENAI_API_KEY");
        let err = resolve_params(&mut p, &db).unwrap_err();
        assert!(err.contains("base_url"), "error was: {err}");
    }

    #[test]
    fn list_models_and_agents_serialize() {
        let db = db_with_fixtures();
        let models: Vec<crate::db::Model> =
            serde_json::from_slice(&handle_list_models(&db).unwrap()).unwrap();
        assert_eq!(models.len(), 2);
        let agents: Vec<crate::db::Agent> =
            serde_json::from_slice(&handle_list_agents(&db).unwrap()).unwrap();
        assert_eq!(agents.len(), 1);
        assert!(agents[0].is_default);
    }

    #[test]
    fn usage_stats_serialize() {
        let db = db_with_fixtures();
        db.record_usage(&UsageRow {
            agent_id: "code".into(),
            model_id: "llama3.2".into(),
            input_tokens: 3,
            output_tokens: 1,
        })
        .unwrap();
        let stats: crate::db::UsageStats =
            serde_json::from_slice(&handle_usage_stats(&db).unwrap()).unwrap();
        assert_eq!(stats.totals.requests, 1);
        assert_eq!(stats.totals.input_tokens, 3);
    }
}
