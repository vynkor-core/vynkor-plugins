//! `search` plugin — web search for other plugins, routed through the
//! `network` plugin's `http_request` action rather than opening its own
//! sockets (same architecture as `ai`/`tts`/`stt` — see ROADMAP.md).
//!
//! Same shape as `ai` (see plugins/ai/src/main.rs): doesn't use the SDK's
//! `Plugin::run`/`serve` loop because `Plugin::on_message` only gets
//! `&mut self`, not `&mut VynkorClient`, and the kernel rejects a second
//! connection under the same `plugin_id`. So this plugin drives its own
//! loop, near-identical to the SDK's `serve()`, calling the handler with the
//! loop's own `&mut VynkorClient` in hand. Sequential, one request at a
//! time — same model `network`, `ai`, `tts`, and `ping-pong-rs` already use.

use search_plugin::handler;
use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus, Envelope, PluginManifest, Pong};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "search";
const PLUGIN_VERSION: &str = "0.1.0";

fn manifest() -> PluginManifest {
    PluginManifest {
        // `network`: search invokes `network`'s gated `http_request` action,
        // and `secrets`: search resolves provider keys from the secrets vault
        // first (`secret_get`, gated by PERMISSION_SECRETS). T-19 requires
        // callers of a gated action to hold its permission too (matches
        // plugin.json `permissions`; Manifest v2 per-action model).
        permissions: vec!["PERMISSION_NETWORK".into(), "PERMISSION_SECRETS".into()],
        actions: vec!["web_search".to_string()],
        action_specs: vynkor_plugin_manifest::action_specs(),
        ..Default::default()
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn handle_action_request(
    client: &mut VynkorClient,
    req: vynkor_sdk::proto::ActionRequest,
) -> Envelope {
    let reply = match req.action.as_str() {
        "web_search" => match handler::handle_web_search(client, &req.params_json).await {
            Ok(data_json) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionOk as i32,
                data_json,
                error: String::new(),
            },
            Err(error) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionError as i32,
                data_json: Vec::new(),
                error,
            },
        },
        other => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionNotFound as i32,
            data_json: Vec::new(),
            error: format!("unknown action: {other}"),
        },
    };
    Envelope {
        payload: Some(envelope::Payload::ActionResponse(reply)),
        ..Default::default()
    }
}

async fn serve(mut client: VynkorClient) -> Result<(), VynkorError> {
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client
        .register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token)
        .await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!(
            "registration rejected: {}",
            ack.reject_reason
        )));
    }
    println!("[{PLUGIN_ID}] registered with kernel");

    loop {
        let env = match client.recv().await {
            Ok(env) => env,
            Err(_) => break, // disconnect / EOF
        };
        match env.payload {
            Some(envelope::Payload::Ping(ping)) => {
                let pong = Envelope {
                    payload: Some(envelope::Payload::Pong(Pong {
                        original_timestamp: ping.timestamp,
                        server_timestamp: unix_millis(),
                    })),
                    ..Default::default()
                };
                let _ = client.send("kernel", pong).await;
            }
            Some(envelope::Payload::PluginShutdown(_)) => break,
            Some(envelope::Payload::Event(event)) => {
                // search declares no event subscriptions; ack defensively so
                // the kernel doesn't retry anything unexpectedly delivered.
                let _ = client.ack_event(&event.event_id).await;
            }
            Some(envelope::Payload::ActionRequest(req)) => {
                let resp = handle_action_request(&mut client, req).await;
                let _ = client.send("kernel", resp).await;
            }
            other => {
                println!("[{PLUGIN_ID}] unhandled message: {other:?}");
            }
        }
    }

    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let socket_path = std::env::var("VYN_SOCKET_PATH")
        .unwrap_or_else(|_| vynkor_wire::socket::default_socket_path());
    let secret = std::env::var("VYN_JWT_SECRET")
        .ok()
        .filter(|s| !s.is_empty());
    let client = match secret {
        Some(s) => VynkorClient::connect_with_secret(&socket_path, s.as_bytes()).await?,
        None => VynkorClient::connect(&socket_path).await?,
    };
    serve(client).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;
    use vynkor_sdk::proto::{ActionResponse as ProtoActionResponse, PluginRegisterAck};

    const VAULT_KEY: &str = "vault-key-123";
    const ENV_DECOY_KEY: &str = "env-decoy-key";

    /// Set the process env the handler reads, exactly once. A single fixed
    /// allowlist (plus one decoy key var to prove vault-wins-over-env) keeps
    /// every parallel test consistent — no test ever mutates env at runtime.
    fn test_env() {
        static ENV: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ENV.get_or_init(|| {
            std::env::set_var(
                "SEARCH_PLUGIN_ALLOWED_KEY_ENVS",
                "SEARCH_BRAVE_KEY,SEARCH_TAVILY_KEY",
            );
            std::env::set_var("SEARCH_BRAVE_KEY", ENV_DECOY_KEY);
        });
    }

    const BRAVE_FIXTURE: &str = r#"{"web":{"results":[
        {"title":"vynkor","url":"https://example.com/vynkor","description":"A plugin kernel"},
        {"title":"vynkor docs","url":"https://example.com/docs","description":"Plugin authoring"}
    ]}}"#;

    const TAVILY_FIXTURE: &str = r#"{"results":[
        {"title":"vynkor","url":"https://example.com/vynkor","content":"A plugin kernel"}
    ]}"#;

    type HttpRequests = Arc<Mutex<Vec<serde_json::Value>>>;
    type SecretGets = Arc<Mutex<Vec<serde_json::Value>>>;

    enum Cmd {
        Call {
            action: String,
            params: serde_json::Value,
            reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
        },
    }

    struct Shim {
        tx: tokio::sync::mpsc::Sender<Cmd>,
        http_requests: HttpRequests,
        secret_gets: SecretGets,
    }

    impl Shim {
        async fn call(
            &self,
            action: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            self.tx
                .send(Cmd::Call {
                    action: action.into(),
                    params,
                    reply: reply_tx,
                })
                .await
                .expect("shim loop died");
            tokio::time::timeout(Duration::from_secs(5), reply_rx)
                .await
                .expect("timed out waiting for plugin reply")
                .expect("shim dropped reply channel")
        }

        async fn http_requests(&self) -> Vec<serde_json::Value> {
            self.http_requests.lock().await.clone()
        }

        async fn secret_gets(&self) -> Vec<serde_json::Value> {
            self.secret_gets.lock().await.clone()
        }
    }

    /// Start the real `serve` loop against a fake kernel over a socket pair.
    /// `secret_data` is the `data_json` the shim returns for `secret_get`;
    /// `http_data` is the `data_json` it returns for `http_request`.
    async fn start_plugin(secret_data: serde_json::Value, http_data: serde_json::Value) -> Shim {
        test_env();
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        tokio::spawn(async move {
            let _ = serve(plugin_client).await;
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<Cmd>(16);
        let http_requests: HttpRequests = Arc::new(Mutex::new(Vec::new()));
        let secret_gets: SecretGets = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_shim(
            kernel_client,
            rx,
            http_requests.clone(),
            secret_gets.clone(),
            secret_data,
            http_data,
        ));
        Shim {
            tx,
            http_requests,
            secret_gets,
        }
    }

    async fn run_shim(
        mut kernel: VynkorClient,
        mut rx: tokio::sync::mpsc::Receiver<Cmd>,
        http_requests: HttpRequests,
        secret_gets: SecretGets,
        secret_data: serde_json::Value,
        http_data: serde_json::Value,
    ) {
        let mut pending: StdHashMap<String, tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>> =
            StdHashMap::new();
        let mut seq: u64 = 0;

        // Registration handshake FIRST, before the command loop: the plugin's
        // register_full treats the very next inbound frame as the ack, so a
        // test command racing ahead of PluginRegister would kill the plugin
        // with "expected PluginRegisterAck". Commands queue in the buffered
        // `rx` until this completes.
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for plugin registration")
                .expect("plugin stream closed before registration");
            match env.payload {
                Some(envelope::Payload::PluginRegister(_)) => {
                    let _ = kernel
                        .send(
                            "search",
                            Envelope {
                                payload: Some(envelope::Payload::PluginRegisterAck(
                                    PluginRegisterAck {
                                        accepted: true,
                                        ..Default::default()
                                    },
                                )),
                                ..Default::default()
                            },
                        )
                        .await;
                    break;
                }
                _ => continue,
            }
        }

        loop {
            tokio::select! {
                env = kernel.recv() => {
                    let env = match env { Ok(e) => e, Err(_) => break };
                    match env.payload {
                        Some(envelope::Payload::ActionRequest(req)) => {
                            // Outbound call from the plugin to the fake
                            // `secrets`/`network` plugins.
                            let params: serde_json::Value =
                                serde_json::from_slice(&req.params_json).unwrap_or(serde_json::Value::Null);
                            let outcome = match req.action.as_str() {
                                "secret_get" => {
                                    secret_gets.lock().await.push(params);
                                    Ok(secret_data.clone())
                                }
                                "http_request" => {
                                    http_requests.lock().await.push(params);
                                    Ok(http_data.clone())
                                }
                                other => Err(format!("unexpected outbound action: {other}")),
                            };
                            let resp = match outcome {
                                Ok(v) => ProtoActionResponse {
                                    action_id: req.action_id,
                                    status: ActionStatus::ActionOk as i32,
                                    data_json: serde_json::to_vec(&v).unwrap(),
                                    error: String::new(),
                                },
                                Err(e) => ProtoActionResponse {
                                    action_id: req.action_id,
                                    status: ActionStatus::ActionError as i32,
                                    data_json: Vec::new(),
                                    error: e,
                                },
                            };
                            let _ = kernel.send("search", Envelope {
                                payload: Some(envelope::Payload::ActionResponse(resp)),
                                ..Default::default()
                            }).await;
                        }
                        Some(envelope::Payload::ActionResponse(resp)) => {
                            if let Some(tx) = pending.remove(&resp.action_id) {
                                let result = if resp.status == ActionStatus::ActionOk as i32 {
                                    serde_json::from_slice::<serde_json::Value>(&resp.data_json)
                                        .map_err(|e| format!("malformed payload: {e}"))
                                } else {
                                    Err(resp.error)
                                };
                                let _ = tx.send(result);
                            }
                        }
                        Some(envelope::Payload::Ping(ping)) => {
                            let _ = kernel.send("search", Envelope {
                                payload: Some(envelope::Payload::Pong(Pong {
                                    original_timestamp: ping.timestamp,
                                    server_timestamp: unix_millis(),
                                })),
                                ..Default::default()
                            }).await;
                        }
                        Some(envelope::Payload::PluginShutdown(_)) => break,
                        _ => {}
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Call { action, params, reply }) => {
                            seq += 1;
                            let action_id = format!("t-{seq}");
                            pending.insert(action_id.clone(), reply);
                            let env = Envelope {
                                payload: Some(envelope::Payload::ActionRequest(
                                    vynkor_sdk::proto::ActionRequest {
                                        action_id,
                                        action,
                                        params_json: serde_json::to_vec(&params).unwrap(),
                                        timeout_ms: 0,
                                        streaming: false,
                                        caller_plugin_id: "tester".into(),
                                    },
                                )),
                                ..Default::default()
                            };
                            let _ = kernel.send("search", env).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    fn secret_found(key: &str) -> serde_json::Value {
        serde_json::json!({"found": true, "value": key})
    }

    fn http_ok(body: &str) -> serde_json::Value {
        serde_json::json!({"status": 200, "body": body, "body_encoding": "utf8"})
    }

    #[tokio::test]
    async fn brave_search_end_to_end_vault_wins_and_header_carries_key() {
        let shim = start_plugin(secret_found(VAULT_KEY), http_ok(BRAVE_FIXTURE)).await;

        let out = shim
            .call(
                "web_search",
                serde_json::json!({
                    "query": "vynkor",
                    "provider": "brave",
                    "api_key_env": "SEARCH_BRAVE_KEY",
                }),
            )
            .await
            .unwrap();

        // Normalized output.
        assert_eq!(out["query"], "vynkor");
        assert_eq!(out["results"].as_array().unwrap().len(), 2);
        assert_eq!(out["results"][0]["title"], "vynkor");
        assert_eq!(out["results"][0]["url"], "https://example.com/vynkor");
        assert_eq!(out["results"][0]["snippet"], "A plugin kernel");

        // The secret_get hop named the allowlisted handle.
        let secret_gets = shim.secret_gets().await;
        assert_eq!(secret_gets.len(), 1);
        assert_eq!(secret_gets[0]["name"], "SEARCH_BRAVE_KEY");

        // The http_request carried the vault key in the auth header — not the
        // decoy env var (vault wins) — and leaked it nowhere else.
        let reqs = shim.http_requests().await;
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0]["method"], "GET");
        assert!(reqs[0]["url"]
            .as_str()
            .unwrap()
            .starts_with("https://api.search.brave.com/res/v1/web/search?q=vynkor&count="));
        assert_eq!(reqs[0]["headers"]["X-Subscription-Token"], VAULT_KEY);
        assert!(!reqs[0]["url"].as_str().unwrap().contains(VAULT_KEY));
        assert!(!reqs[0]["url"].as_str().unwrap().contains(ENV_DECOY_KEY));
    }

    #[tokio::test]
    async fn tavily_search_end_to_end() {
        let shim = start_plugin(secret_found(VAULT_KEY), http_ok(TAVILY_FIXTURE)).await;

        let out = shim
            .call(
                "web_search",
                serde_json::json!({
                    "query": "vynkor",
                    "provider": "tavily",
                    "api_key_env": "SEARCH_TAVILY_KEY",
                    "count": 3,
                }),
            )
            .await
            .unwrap();

        assert_eq!(out["query"], "vynkor");
        assert_eq!(out["results"].as_array().unwrap().len(), 1);
        assert_eq!(out["results"][0]["snippet"], "A plugin kernel");

        let reqs = shim.http_requests().await;
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0]["method"], "POST");
        assert_eq!(reqs[0]["url"], "https://api.tavily.com/search");
        assert_eq!(reqs[0]["headers"]["Authorization"], format!("Bearer {VAULT_KEY}"));
        let body: serde_json::Value = serde_json::from_str(reqs[0]["body"].as_str().unwrap()).unwrap();
        assert_eq!(body["query"], "vynkor");
        assert_eq!(body["max_results"], 3);
        assert!(!reqs[0]["body"].as_str().unwrap().contains(VAULT_KEY));
    }

    #[tokio::test]
    async fn non_2xx_status_maps_to_clear_error_without_key_leak() {
        let shim = start_plugin(
            secret_found(VAULT_KEY),
            serde_json::json!({"status": 401, "body": "unauthorized", "body_encoding": "utf8"}),
        )
        .await;

        let err = shim
            .call(
                "web_search",
                serde_json::json!({
                    "query": "vynkor",
                    "provider": "brave",
                    "api_key_env": "SEARCH_BRAVE_KEY",
                }),
            )
            .await
            .unwrap_err();

        assert!(err.contains("HTTP 401"), "error was: {err}");
        assert!(err.contains("unauthorized"), "error was: {err}");
        assert!(!err.contains(VAULT_KEY), "key leaked into error: {err}");
    }

    #[tokio::test]
    async fn unallowlisted_key_env_is_rejected() {
        let shim = start_plugin(secret_found(VAULT_KEY), http_ok(BRAVE_FIXTURE)).await;

        let err = shim
            .call(
                "web_search",
                serde_json::json!({
                    "query": "vynkor",
                    "provider": "brave",
                    "api_key_env": "UNLISTED_KEY",
                }),
            )
            .await
            .unwrap_err();

        assert!(err.contains("allowlist"), "error was: {err}");
        assert!(!err.contains(VAULT_KEY), "key leaked into error: {err}");
        // Rejected before any vault/env resolution: no secret_get hop fired.
        assert!(shim.secret_gets().await.is_empty());
    }

    #[tokio::test]
    async fn missing_key_is_error_without_leak() {
        // Vault has no value, and SEARCH_TAVILY_KEY is not set in the env.
        let shim = start_plugin(
            serde_json::json!({"found": false}),
            http_ok(BRAVE_FIXTURE),
        )
        .await;

        let err = shim
            .call(
                "web_search",
                serde_json::json!({
                    "query": "vynkor",
                    "provider": "tavily",
                    "api_key_env": "SEARCH_TAVILY_KEY",
                }),
            )
            .await
            .unwrap_err();

        assert!(err.contains("neither in the secrets vault"), "error was: {err}");
        // No http_request was ever sent.
        assert!(shim.http_requests().await.is_empty());
    }
}

#[cfg(test)]
mod manifest_specs_tests {
    use serde_json::Value;

    // Regression guard: an action that reaches the model with an empty
    // description gets dropped by the agent's embedding filter, and one
    // with no risk label falls through to the agent's name-shaped
    // inference instead of this plugin's own judgement.
    //
    // Reads the shipped plugin.json directly, not action_specs(): that
    // resolves the manifest via current_exe(), which in a dev-build test
    // binary finds nothing and returns an empty list — it cannot tell
    // correct wiring from no wiring at all.
    #[test]
    fn shipped_manifest_documents_and_risk_rates_every_declared_action() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        assert_eq!(
            specs.len(),
            parsed["actions"].as_array().unwrap().len(),
            "every declared action must produce a spec"
        );
        let undocumented: Vec<&str> =
            specs.iter().filter(|s| s.description.is_empty()).map(|s| s.name.as_str()).collect();
        assert!(undocumented.is_empty(), "actions missing a description: {undocumented:?}");
        let unrisked: Vec<&str> = specs
            .iter()
            .filter(|s| s.risk == vynkor_sdk::proto::ActionRisk::Unknown as i32)
            .map(|s| s.name.as_str())
            .collect();
        assert!(unrisked.is_empty(), "actions missing a risk label: {unrisked:?}");
    }
}
