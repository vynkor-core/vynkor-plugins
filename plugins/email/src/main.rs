//! `email` plugin — SMTP email sending for other plugins. Resolves the SMTP
//! password vault-first via the `secrets` plugin's `secret_get` action, then
//! sends with `lettre`. A `EMAIL_PLUGIN_SMTP_STUB=true` env keeps tests and
//! smoke-tests offline (no real SMTP).
//!
//! Same shape as `search`/`ai` (see plugins/search/src/main.rs): doesn't use
//! the SDK's `Plugin::run`/`serve` loop because `Plugin::on_message` only gets
//! `&mut self`, not `&mut VynkorClient`, and the kernel rejects a second
//! connection under the same `plugin_id`. So this plugin drives its own loop,
//! near-identical to the SDK's `serve()`, calling the handler with the loop's
//! own `&mut VynkorClient` in hand. Sequential, one request at a time.

use email_plugin::handler;
use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus, Envelope, PluginManifest, Pong};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "email";
const PLUGIN_VERSION: &str = "0.1.0";

fn manifest() -> PluginManifest {
    PluginManifest {
        // `secrets`: email resolves the SMTP password from the secrets vault
        // first (`secret_get`, gated by PERMISSION_SECRETS). T-19 requires
        // callers of a gated action to hold its permission too (matches
        // plugin.json `permissions`; Manifest v2 per-action model). No
        // PERMISSION_NETWORK — email opens its own SMTP socket, it does not
        // call `network`'s `http_request`.
        permissions: vec!["PERMISSION_SECRETS".into()],
        actions: vec!["email_send".to_string(), "email_list".to_string()],
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
        "email_send" => match handler::handle_email_send(client, &req.params_json).await {
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
        "email_list" => match handler::handle_email_list(client, &req.params_json).await {
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
                // email declares no event subscriptions; ack defensively so
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

    const VAULT_PASS: &str = "vault-pass-123";
    const ENV_DECOY_PASS: &str = "env-decoy-pass";

    /// Set the process env the handler reads, exactly once. A single fixed
    /// allowlist (two names: one set as a decoy env var to prove vault-wins,
    /// one left unset for the missing-credential case) plus stub mode keeps
    /// every parallel test consistent — no test ever mutates env at runtime.
    fn test_env() {
        static ENV: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ENV.get_or_init(|| {
            std::env::set_var(
                "EMAIL_PLUGIN_ALLOWED_CRED_ENVS",
                "EMAIL_SMTP_PASS,EMAIL_SMTP_PASS_ALT",
            );
            std::env::set_var("EMAIL_SMTP_PASS", ENV_DECOY_PASS);
            std::env::set_var("EMAIL_PLUGIN_SMTP_STUB", "true");
        });
    }

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

        async fn secret_gets(&self) -> Vec<serde_json::Value> {
            self.secret_gets.lock().await.clone()
        }
    }

    /// Start the real `serve` loop against a fake kernel over a socket pair.
    /// `secret_data` is the `data_json` the shim returns for `secret_get`.
    async fn start_plugin(secret_data: serde_json::Value) -> Shim {
        test_env();
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        tokio::spawn(async move {
            let _ = serve(plugin_client).await;
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<Cmd>(16);
        let secret_gets: SecretGets = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_shim(kernel_client, rx, secret_gets.clone(), secret_data));
        Shim { tx, secret_gets }
    }

    async fn run_shim(
        mut kernel: VynkorClient,
        mut rx: tokio::sync::mpsc::Receiver<Cmd>,
        secret_gets: SecretGets,
        secret_data: serde_json::Value,
    ) {
        let mut pending: StdHashMap<
            String,
            tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
        > = StdHashMap::new();
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
                            "email",
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
                            // `secrets` plugin.
                            let params: serde_json::Value =
                                serde_json::from_slice(&req.params_json).unwrap_or(serde_json::Value::Null);
                            let outcome = match req.action.as_str() {
                                "secret_get" => {
                                    secret_gets.lock().await.push(params);
                                    Ok(secret_data.clone())
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
                            let _ = kernel.send("email", Envelope {
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
                            let _ = kernel.send("email", Envelope {
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
                            let _ = kernel.send("email", env).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    fn secret_found(value: &str) -> serde_json::Value {
        serde_json::json!({"found": true, "value": value})
    }

    #[tokio::test]
    async fn email_send_end_to_end_vault_wins() {
        let shim = start_plugin(secret_found(VAULT_PASS)).await;

        let out = shim
            .call(
                "email_send",
                serde_json::json!({
                    "to": "user@example.com",
                    "subject": "Hello",
                    "body": "Hello there",
                    "credentials_env": "EMAIL_SMTP_PASS",
                }),
            )
            .await
            .unwrap();

        // Stub-mode success shape.
        assert_eq!(out["to"], "user@example.com");
        assert_eq!(out["subject"], "Hello");
        assert_eq!(out["stubbed"], true);
        assert!(
            out["message_id"].as_str().unwrap().starts_with("stub-"),
            "message_id was: {}",
            out["message_id"]
        );

        // Vault-first: exactly one secret_get hop fired, naming the
        // allowlisted handle. (EMAIL_SMTP_PASS is also set as a decoy env
        // var, so a working env-first path would have skipped the vault.)
        let secret_gets = shim.secret_gets().await;
        assert_eq!(secret_gets.len(), 1);
        assert_eq!(secret_gets[0]["name"], "EMAIL_SMTP_PASS");

        // Neither the resolved vault password nor the decoy env value leaks
        // into the response.
        let raw = serde_json::to_string(&out).unwrap();
        assert!(!raw.contains(VAULT_PASS), "vault password leaked: {raw}");
        assert!(!raw.contains(ENV_DECOY_PASS), "decoy env leaked: {raw}");
    }

    #[tokio::test]
    async fn unallowlisted_cred_env_rejected() {
        let shim = start_plugin(secret_found(VAULT_PASS)).await;

        let err = shim
            .call(
                "email_send",
                serde_json::json!({
                    "to": "user@example.com",
                    "subject": "Hello",
                    "body": "Hello there",
                    "credentials_env": "UNLISTED_CRED",
                }),
            )
            .await
            .unwrap_err();

        assert!(err.contains("allowlist"), "error was: {err}");
        assert!(!err.contains(VAULT_PASS), "password leaked into error: {err}");
        // Rejected before any vault/env resolution: no secret_get hop fired.
        assert!(shim.secret_gets().await.is_empty());
    }

    #[tokio::test]
    async fn missing_cred_is_error() {
        // Vault has no value, and EMAIL_SMTP_PASS_ALT is not set in the env.
        let shim = start_plugin(serde_json::json!({"found": false})).await;

        let err = shim
            .call(
                "email_send",
                serde_json::json!({
                    "to": "user@example.com",
                    "subject": "Hello",
                    "body": "Hello there",
                    "credentials_env": "EMAIL_SMTP_PASS_ALT",
                }),
            )
            .await
            .unwrap_err();

        assert!(
            err.contains("neither in the secrets vault"),
            "error was: {err}"
        );
        assert!(!err.contains(VAULT_PASS), "password leaked into error: {err}");
    }

    #[tokio::test]
    async fn invalid_email_rejected() {
        let shim = start_plugin(secret_found(VAULT_PASS)).await;

        let err = shim
            .call(
                "email_send",
                serde_json::json!({
                    "to": "not-an-email",
                    "subject": "Hello",
                    "body": "Hello there",
                    "credentials_env": "EMAIL_SMTP_PASS",
                }),
            )
            .await
            .unwrap_err();

        assert!(err.contains("invalid recipient"), "error was: {err}");
        // Rejected at parse time, before any vault resolution.
        assert!(shim.secret_gets().await.is_empty());
    }

    #[tokio::test]
    async fn missing_subject_rejected() {
        let shim = start_plugin(secret_found(VAULT_PASS)).await;

        let err = shim
            .call(
                "email_send",
                serde_json::json!({
                    "to": "user@example.com",
                    "body": "Hello there",
                    "credentials_env": "EMAIL_SMTP_PASS",
                }),
            )
            .await
            .unwrap_err();

        assert!(err.contains("subject"), "error was: {err}");
        assert!(shim.secret_gets().await.is_empty());
    }

    #[tokio::test]
    async fn email_list_end_to_end_stub() {
        let shim = start_plugin(secret_found(VAULT_PASS)).await;

        let out = shim
            .call(
                "email_list",
                serde_json::json!({
                    "imap_user": "user@example.com",
                    "credentials_env": "EMAIL_SMTP_PASS",
                    "mailbox": "INBOX",
                    "limit": 2
                }),
            )
            .await
            .unwrap();

        assert_eq!(out["mailbox"], "INBOX");
        assert_eq!(out["count"], 2);
        assert_eq!(out["stubbed"], true);
        assert_eq!(out["emails"].as_array().unwrap().len(), 2);
        assert_eq!(out["emails"][0]["subject"], "Stub email 1");
        assert_eq!(out["emails"][1]["subject"], "Stub email 2");

        let secret_gets = shim.secret_gets().await;
        assert_eq!(secret_gets.len(), 1);
        assert_eq!(secret_gets[0]["name"], "EMAIL_SMTP_PASS");

        let raw = serde_json::to_string(&out).unwrap();
        assert!(!raw.contains(VAULT_PASS), "vault password leaked: {raw}");
        assert!(!raw.contains(ENV_DECOY_PASS), "decoy leaked: {raw}");
    }

    #[tokio::test]
    async fn email_list_unallowlisted_rejected() {
        let shim = start_plugin(secret_found(VAULT_PASS)).await;
        let err = shim
            .call(
                "email_list",
                serde_json::json!({
                    "imap_user": "user@example.com",
                    "credentials_env": "UNLISTED_CRED",
                }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("allowlist"), "error was: {err}");
        assert!(shim.secret_gets().await.is_empty());
    }

    #[tokio::test]
    async fn email_list_missing_imap_user_rejected() {
        let shim = start_plugin(secret_found(VAULT_PASS)).await;
        let err = shim
            .call(
                "email_list",
                serde_json::json!({
                    "credentials_env": "EMAIL_SMTP_PASS",
                }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("imap_user"), "error was: {err}");
        assert!(shim.secret_gets().await.is_empty());
    }

    #[tokio::test]
    async fn email_list_invalid_mailbox_rejected() {
        let shim = start_plugin(secret_found(VAULT_PASS)).await;
        let err = shim
            .call(
                "email_list",
                serde_json::json!({
                    "imap_user": "user@example.com",
                    "credentials_env": "EMAIL_SMTP_PASS",
                    "mailbox": "../INBOX"
                }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("mailbox"), "error was: {err}");
        assert!(shim.secret_gets().await.is_empty());
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
