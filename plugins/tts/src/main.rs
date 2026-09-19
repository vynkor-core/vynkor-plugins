//! `tts` plugin — text-to-speech for other plugins.
//!
//! Same shape as `ai` (see plugins/ai/src/main.rs): doesn't use the SDK's
//! `Plugin::run`/`serve` loop because `Plugin::on_message` only gets
//! `&mut self`, not `&mut VynkorClient`, and the kernel rejects a second
//! connection under the same `plugin_id`. So this plugin drives its own
//! loop, near-identical to the SDK's `serve()`, calling the handlers with
//! the loop's own `&mut VynkorClient` in hand. Sequential, one request at
//! a time — same model `network`, `ai`, and `ping-pong-rs` already use.
//!
//! Cloud providers (`openai`, `elevenlabs`) route HTTP through the
//! `network` plugin's `http_request` action. The local provider (`sherpa`)
//! synthesizes in-process and never touches the network. See ROADMAP.md
//! for the design rationale.

use tts_plugin::handler;
use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus, Envelope, PluginManifest, Pong};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "tts";
const PLUGIN_VERSION: &str = "0.1.1";

/// Comma-separated allowlist of `ipc_targets` for `tts_speak` streaming.
/// The kernel gates peer-to-peer unicast per-target (T-04): a target not
/// listed here gets `ERR_PERMISSION_DENIED`. Default-deny — unset means
/// `tts_speak` can only address peers the operator explicitly allows
/// (e.g. `device.phone.speaker` for a remote speaker, D-12/D-14).
const IPC_TARGETS_ENV: &str = "TTS_PLUGIN_IPC_TARGETS";

fn manifest() -> PluginManifest {
    let ipc_targets: Vec<String> = std::env::var(IPC_TARGETS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    PluginManifest {
        // `network`: cloud providers invoke `network`'s gated `http_request`,
        // and T-19 requires callers of a gated action to hold its permission
        // too (matches plugin.json `permissions`; Manifest v2 per-action model).
        // `audio_stream`: `tts_speak` streams `AudioStreamChunk`s to a peer
        // (PERMISSION_AUDIO_STREAM, proto v1.6).
        // `secrets`: cloud providers resolve their API keys from the
        // `secrets` plugin's vault first (gated `secret_get`), and T-19
        // requires callers of a gated action to hold its permission too.
        permissions: vec![
            "PERMISSION_NETWORK".into(),
            "PERMISSION_AUDIO_STREAM".into(),
            "PERMISSION_IPC_SEND".into(),
            "PERMISSION_SECRETS".into(),
        ],
        actions: vec![
            "tts_synthesize".to_string(),
            "tts_voices".to_string(),
            "tts_speak".to_string(),
            "tts_speak_stream".to_string(),
        ],
        action_specs: vynkor_plugin_manifest::action_specs(),
        ipc_targets,
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
        "tts_synthesize" => match handler::handle_tts_synthesize(client, &req.params_json).await {
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
        "tts_voices" => match handler::handle_tts_voices(client, &req.params_json).await {
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
        "tts_speak" => match handler::handle_tts_speak(client, &req.params_json).await {
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
        "tts_speak_stream" => match handler::handle_tts_speak_stream(client, &req.params_json).await
        {
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
                // tts declares no event subscriptions; ack defensively so
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
