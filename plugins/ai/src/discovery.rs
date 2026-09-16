//! Automatic model discovery. Pulls the model lists of configured providers
//! through `network`'s `http_request` action and upserts them into the
//! database, so the phone never has to type a model list by hand.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use vynkor_sdk::proto::ActionStatus;

use crate::config::DiscoverySource;
use crate::db::{AiDb, Model};
use crate::outbound::ActionCaller;

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Discovered {
    pub discovered: usize,
    pub updated: usize,
    pub errors: Vec<String>,
}

/// `network`'s `http_request` response shape — only what discovery needs.
#[derive(serde::Deserialize)]
struct NetResponse {
    status: u16,
    body: String,
    body_encoding: String,
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Pull the model list of every configured discovery source and upsert the
/// results into `db`. Returns per-source errors, never a hard failure.
pub async fn refresh_models(
    caller: &mut impl ActionCaller,
    db: &AiDb,
    sources: &[DiscoverySource],
) -> Result<Discovered, String> {
    let mut result = Discovered::default();
    for src in sources {
        match fetch_ids(caller, src).await {
            Ok(ids) => {
                let base = completion_base(&src.provider, &src.base_url);
                let discovered_at = Some(now_millis());
                for id in ids {
                    let existed = db
                        .get_model(&id)
                        .map_err(|e| format!("db error: {e}"))?
                        .is_some();
                    // Preserve an operator-configured default flag: discovery
                    // never promotes nor demotes a model as default.
                    let is_default = db
                        .get_model(&id)
                        .map_err(|e| format!("db error: {e}"))?
                        .map(|m| m.is_default)
                        .unwrap_or(false);
                    db.upsert_model(&Model {
                        id: id.clone(),
                        provider: store_provider(&src.provider),
                        base_url: base.clone(),
                        api_key_env: src.api_key_env.clone(),
                        is_default,
                        discovered_at,
                        last_seen: 0,
                    })
                    .map_err(|e| format!("db error: {e}"))?;
                    if existed {
                        result.updated += 1;
                    } else {
                        result.discovered += 1;
                    }
                }
            }
            Err(e) => result.errors.push(format!("{}: {e}", src.base_url)),
        }
    }
    Ok(result)
}

/// The completion `base_url` a discovered model is called with. Ollama's tag
/// endpoint lives on `:11434` while chat completions are served at `:11434/v1`;
/// OpenAI-compatible sources already carry their `/v1`.
fn completion_base(provider: &str, base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if provider == "ollama" {
        format!("{trimmed}/v1")
    } else {
        trimmed.to_string()
    }
}

/// Wire provider a discovered model is served with. `ollama` is only a
/// discovery source — its models speak the openai-compatible API, so they
/// are stored as `openai` (what `chat_completion` resolution expects).
fn store_provider(provider: &str) -> String {
    match provider {
        "ollama" => "openai".to_string(),
        other => other.to_string(),
    }
}

async fn fetch_ids(
    caller: &mut impl ActionCaller,
    src: &DiscoverySource,
) -> Result<Vec<String>, String> {
    let (url, headers) = match src.provider.as_str() {
        "ollama" => (
            format!("{}/api/tags", src.base_url.trim_end_matches('/')),
            HashMap::new(),
        ),
        "openai" => {
            let key = crate::key_resolve::resolve_api_key(caller, &src.api_key_env)
                .await
                .unwrap_or_default();
            let mut h = HashMap::new();
            if !key.is_empty() {
                h.insert("Authorization".to_string(), format!("Bearer {key}"));
            }
            (format!("{}/models", src.base_url.trim_end_matches('/')), h)
        }
        other => return Err(format!("unsupported discovery provider: {other}")),
    };

    let body = http_get(caller, &url, &headers, 15_000).await?;
    match src.provider.as_str() {
        "ollama" => parse_ollama_tags(&body),
        _ => parse_openai_models(&body),
    }
}

async fn http_get(
    caller: &mut impl ActionCaller,
    url: &str,
    headers: &HashMap<String, String>,
    timeout_ms: u64,
) -> Result<String, String> {
    let params = serde_json::json!({
        "method": "GET",
        "url": url,
        "headers": headers,
        "body": null,
        "timeout_ms": timeout_ms,
        "max_retries": 1,
        "retry_backoff_ms": 500,
        "follow_redirects": true,
        "max_redirects": 5,
    });
    let resp = caller
        .call_action(
            "http_request",
            &serde_json::to_vec(&params).unwrap_or_default(),
            timeout_ms as u32,
        )
        .await
        .map_err(|e| format!("network call failed: {e}"))?;
    if resp.status != ActionStatus::ActionOk as i32 {
        return Err(format!("network plugin error: {}", resp.error));
    }
    let net: NetResponse = serde_json::from_slice(&resp.data_json)
        .map_err(|e| format!("malformed network response: {e}"))?;
    if !(200..300).contains(&net.status) {
        return Err(format!(
            "provider returned HTTP {}: {}",
            net.status, net.body
        ));
    }
    if net.body_encoding == "base64" {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&net.body)
            .map_err(|e| format!("malformed base64 body: {e}"))?;
        String::from_utf8(bytes).map_err(|e| format!("body is not utf-8: {e}"))
    } else {
        Ok(net.body)
    }
}

/// `GET /api/tags` → `{"models": [{"name": "llama3.2:latest", ...}]}`.
/// Normalizes `:latest` away so the model id matches Ollama's default alias.
fn parse_ollama_tags(body: &str) -> Result<Vec<String>, String> {
    #[derive(serde::Deserialize)]
    struct Tags {
        #[serde(default)]
        models: Vec<Tag>,
    }
    #[derive(serde::Deserialize)]
    struct Tag {
        name: String,
    }
    let tags: Tags = serde_json::from_str(body)
        .map_err(|e| format!("malformed ollama /api/tags response: {e}"))?;
    Ok(tags
        .models
        .into_iter()
        .map(|t| {
            t.name
                .strip_suffix(":latest")
                .unwrap_or(&t.name)
                .to_string()
        })
        .collect())
}

/// `GET /models` → `{"data": [{"id": "gpt-4o", ...}]}`.
fn parse_openai_models(body: &str) -> Result<Vec<String>, String> {
    #[derive(serde::Deserialize)]
    struct List {
        #[serde(default)]
        data: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        id: String,
    }
    let list: List = serde_json::from_str(body)
        .map_err(|e| format!("malformed openai /models response: {e}"))?;
    Ok(list.data.into_iter().map(|e| e.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_tags_parse_and_strip_latest() {
        let body = r#"{"models":[{"name":"llama3.2:latest"},{"name":"qwen2.5-coder:7b"}]}"#;
        let ids = parse_ollama_tags(body).unwrap();
        assert_eq!(
            ids,
            vec!["llama3.2".to_string(), "qwen2.5-coder:7b".to_string()]
        );
    }

    #[test]
    fn ollama_tags_malformed() {
        assert!(parse_ollama_tags("nope").is_err());
    }

    #[test]
    fn openai_models_parse() {
        let body = r#"{"object":"list","data":[{"id":"gpt-4o"},{"id":"gpt-4o-mini"}]}"#;
        let ids = parse_openai_models(body).unwrap();
        assert_eq!(ids, vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]);
    }

    #[test]
    fn completion_base_normalizes_ollama() {
        assert_eq!(
            completion_base("ollama", "http://localhost:11434"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            completion_base("ollama", "http://localhost:11434/"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            completion_base("openai", "https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn discovered_ollama_models_are_stored_as_openai() {
        assert_eq!(store_provider("ollama"), "openai");
        assert_eq!(store_provider("openai"), "openai");
        assert_eq!(store_provider("anthropic"), "anthropic");
    }
}
