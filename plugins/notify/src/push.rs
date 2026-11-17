//! `push_send` (INT-02): deliver a notification to the phone through ntfy or
//! Gotify, routed via the `network` plugin's gated `http_request` action
//! (same T-19 caller model as `ai`/`search` — this plugin holds
//! `PERMISSION_NETWORK` and never opens sockets itself).
//!
//! Publishing uses the JSON APIs so arbitrary UTF-8 titles/messages need no
//! header encoding: ntfy `POST {server}/` with a topic object, Gotify
//! `POST {server}/message` with the app token in a header (kept out of URLs
//! and logs). Server hosts are operator-allowlisted (`NOTIFY_PLUGIN_PUSH_SERVERS`,
//! default `ntfy.sh`) — a caller-chosen host would turn notify into an
//! exfiltration channel.

use vynkor_sdk::VynkorClient;

use crate::request::{PushParams, PushProvider};

/// `network`'s `http_request` response shape — only what push needs.
#[derive(serde::Deserialize)]
struct NetworkHttpResponse {
    status: u16,
    body: String,
}

/// Resolved outbound configuration for one push (pure — unit-testable).
pub struct PushRequest {
    pub http: serde_json::Value,
    pub timeout_ms: u64,
}

/// Build the `http_request` params for the target provider.
pub fn build_push_http(params: &PushParams, server: &str, token: &str) -> PushRequest {
    let provider = match params.provider.as_deref() {
        Some("gotify") => PushProvider::Gotify,
        _ => PushProvider::Ntfy,
    };
    let scheme = "https";
    let mut headers = serde_json::Map::new();
    headers.insert("Content-Type".into(), "application/json".into());
    match provider {
        PushProvider::Ntfy => {
            if !token.is_empty() {
                headers.insert("Authorization".into(), format!("Bearer {token}").into());
            }
            let mut body = serde_json::Map::new();
            body.insert(
                "topic".into(),
                serde_json::Value::String(params.topic.clone().unwrap_or_default()),
            );
            body.insert(
                "message".into(),
                serde_json::Value::String(params.message.clone()),
            );
            if !params.title.is_empty() {
                body.insert(
                    "title".into(),
                    serde_json::Value::String(params.title.clone()),
                );
            }
            if let Some(priority) = params.priority {
                body.insert("priority".into(), serde_json::json!(priority));
            }
            if !params.tags.is_empty() {
                body.insert("tags".into(), serde_json::json!(params.tags));
            }
            PushRequest {
                http: serde_json::json!({
                    "method": "POST",
                    "url": format!("{scheme}://{server}/"),
                    "headers": headers,
                    // network's http_request carries bodies as strings.
                    "body": serde_json::to_string(&serde_json::Value::Object(body))
                        .unwrap_or_default(),
                    "timeout_ms": timeout_of(params),
                }),
                timeout_ms: timeout_of(params),
            }
        }
        PushProvider::Gotify => {
            if !token.is_empty() {
                headers.insert(
                    "X-Gotify-Token".into(),
                    serde_json::Value::String(token.into()),
                );
            }
            let mut body = serde_json::Map::new();
            body.insert(
                "message".into(),
                serde_json::Value::String(params.message.clone()),
            );
            if !params.title.is_empty() {
                body.insert(
                    "title".into(),
                    serde_json::Value::String(params.title.clone()),
                );
            }
            if let Some(priority) = params.priority {
                body.insert("priority".into(), serde_json::json!(priority as i64));
            }
            PushRequest {
                http: serde_json::json!({
                    "method": "POST",
                    "url": format!("{scheme}://{server}/message"),
                    "headers": headers,
                    // network's http_request carries bodies as strings.
                    "body": serde_json::to_string(&serde_json::Value::Object(body))
                        .unwrap_or_default(),
                    "timeout_ms": timeout_of(params),
                }),
                timeout_ms: timeout_of(params),
            }
        }
    }
}

fn timeout_of(params: &PushParams) -> u64 {
    params.timeout_ms.unwrap_or(10_000)
}

/// Handle one `push_send` action end to end: build the request, send it
/// through `network`, map the response. Never includes tokens in errors.
pub async fn handle_push_send(
    client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let allowed = crate::request::parse_push_servers(
        &std::env::var(crate::request::PUSH_SERVERS_ENV).unwrap_or_default(),
    );
    let params = crate::request::parse_push_params(params_json, &allowed)?;

    let server = params.server.clone().unwrap_or_else(|| allowed[0].clone());
    let provider = match params.provider.as_deref() {
        Some("gotify") => PushProvider::Gotify,
        _ => PushProvider::Ntfy,
    };
    let token_env = match provider {
        PushProvider::Ntfy => crate::request::NTFY_TOKEN_ENV,
        PushProvider::Gotify => crate::request::GOTIFY_TOKEN_ENV,
    };
    let token = std::env::var(token_env).unwrap_or_default();

    let push = build_push_http(&params, &server, token.trim());
    let encoded = serde_json::to_vec(&push.http)
        .map_err(|e| format!("failed to encode http_request params: {e}"))?;

    let response = client
        .send_action("http_request", &encoded, push.timeout_ms as u32)
        .await
        .map_err(|e| format!("network plugin call failed: {e}"))?;
    if response.status != vynkor_sdk::proto::ActionStatus::ActionOk as i32 {
        return Err(format!("network plugin error: {}", response.error));
    }
    let net: NetworkHttpResponse = serde_json::from_slice(&response.data_json)
        .map_err(|e| format!("malformed network response: {e}"))?;
    if !(200..300).contains(&net.status) {
        let snippet: String = net.body.chars().take(200).collect();
        return Err(format!(
            "push server returned HTTP {}: {snippet}",
            net.status
        ));
    }

    serde_json::to_vec(&serde_json::json!({
        "pushed": true,
        "provider": provider.as_str(),
        "server": server,
        "status": net.status,
    }))
    .map_err(|e| format!("failed to encode response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{
        parse_push_params, parse_push_servers, GOTIFY_TOKEN_ENV, NTFY_TOKEN_ENV, PUSH_SERVERS_ENV,
    };
    use serde_json::json;

    fn servers() -> Vec<String> {
        parse_push_servers("ntfy.sh, push.self.example:8443")
    }

    fn base_params(v: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn allowlist_parsing_and_matching() {
        let allowed = servers();
        assert_eq!(allowed, vec!["ntfy.sh", "push.self.example:8443"]);
        assert!(crate::request::is_allowed_push_server("NTFY.SH", &allowed));
        assert!(crate::request::is_allowed_push_server(
            "push.self.example:8443",
            &allowed
        ));
        assert!(!crate::request::is_allowed_push_server(
            "evil.example",
            &allowed
        ));
        assert_eq!(
            parse_push_servers(""),
            vec!["ntfy.sh"],
            "empty operator env falls back to the default host"
        );
    }

    #[test]
    fn parses_defaults_and_validates() {
        let p = parse_push_params(
            &base_params(json!({"message": "hi", "topic": "vyn"})),
            &servers(),
        )
        .unwrap();
        assert_eq!(p.topic.as_deref(), Some("vyn"));
        assert!(
            p.server.is_none(),
            "server defaults to allowlist[0] at send time"
        );

        let err =
            parse_push_params(&base_params(json!({"message": "hi"})), &servers()).unwrap_err();
        assert!(err.contains("topic"), "ntfy requires a topic: {err}");

        let err = parse_push_params(&base_params(json!({"message": ""})), &servers()).unwrap_err();
        assert!(err.contains("non-empty message"), "{err}");

        let err = parse_push_params(
            &base_params(json!({"message": "m", "topic": "bad topic!"})),
            &servers(),
        )
        .unwrap_err();
        assert!(err.contains("topic"), "{err}");

        let err = parse_push_params(
            &base_params(json!({"message": "m", "server": "evil.example"})),
            &servers(),
        )
        .unwrap_err();
        assert!(err.contains(PUSH_SERVERS_ENV), "{err}");

        let err = parse_push_params(
            &base_params(json!({"message": "m", "provider": "gotify", "topic": "t"})),
            &servers(),
        )
        .unwrap_err();
        assert!(err.contains("ntfy-only"), "{err}");

        let err = parse_push_params(
            &base_params(json!({"message": "m", "topic": "vyn", "priority": 9})),
            &servers(),
        )
        .unwrap_err();
        assert!(err.contains("priority"), "{err}");
    }

    #[test]
    fn builds_ntfy_json_publish() {
        let params = parse_push_params(
            &base_params(json!({
                "topic": "vyn",
                "title": "Build",
                "message": "done",
                "priority": 4,
                "tags": ["white_check_mark"],
            })),
            &servers(),
        )
        .unwrap();
        let req = build_push_http(&params, "ntfy.sh", "tk-123");
        assert_eq!(req.http["url"], "https://ntfy.sh/");
        assert_eq!(req.http["method"], "POST");
        assert_eq!(req.http["headers"]["Authorization"], "Bearer tk-123");
        assert!(
            req.http["body"].is_string(),
            "body must be a JSON-encoded string for network"
        );
        let decoded: serde_json::Value =
            serde_json::from_str(req.http["body"].as_str().unwrap()).unwrap();
        assert_eq!(decoded["topic"], "vyn");
        assert_eq!(decoded["title"], "Build");
        assert_eq!(decoded["message"], "done");
        assert_eq!(decoded["priority"], 4);
        assert_eq!(decoded["tags"][0], "white_check_mark");
        assert_eq!(req.http["timeout_ms"], 10_000);
    }

    #[test]
    fn builds_gotify_with_header_token_and_no_topic() {
        let params = parse_push_params(
            &base_params(json!({
                "provider": "gotify",
                "server": "push.self.example:8443",
                "title": "t",
                "message": "m",
            })),
            &servers(),
        )
        .unwrap();
        let req = build_push_http(&params, "push.self.example:8443", "app-token");
        assert_eq!(req.http["url"], "https://push.self.example:8443/message");
        assert_eq!(req.http["headers"]["X-Gotify-Token"], "app-token");
        let decoded: serde_json::Value =
            serde_json::from_str(req.http["body"].as_str().unwrap()).unwrap();
        assert!(decoded.get("topic").is_none());
        assert!(decoded.get("priority").is_none());
        assert_eq!(decoded["message"], "m");
    }

    #[test]
    fn omits_auth_headers_when_tokens_unset() {
        let params = parse_push_params(
            &base_params(json!({"message": "m", "topic": "t"})),
            &servers(),
        )
        .unwrap();
        let req = build_push_http(&params, "ntfy.sh", "");
        assert!(req.http["headers"].get("Authorization").is_none());
        assert_eq!(
            std::env::var(NTFY_TOKEN_ENV).unwrap_or_default(),
            "",
            "test env must not carry a real token"
        );
        let _ = GOTIFY_TOKEN_ENV;
    }
}
