//! `stt` plugin — speech-to-text for other plugins.
//!
//! Same shape as `tts` (see plugins/tts/src/main.rs): doesn't use the
//! SDK's `Plugin::run`/`serve` loop because `Plugin::on_message` only gets
//! `&mut self`, not `&mut VynkorClient`, and the kernel rejects a second
//! connection under the same `plugin_id`. So this plugin drives its own
//! loop, near-identical to the SDK's `serve()`, calling the handlers with
//! the loop's own `&mut VynkorClient` in hand. Sequential, one request at
//! a time — same model `network`, `ai`, `tts`, and `ping-pong-rs` already
//! use.
//!
//! The cloud provider (`openai`) routes its multipart upload through the
//! `network` plugin's `http_request` action. The local provider (`sherpa`)
//! transcribes in-process and never touches the network. See ROADMAP.md
//! for the design rationale.

use stt_plugin::handler;
use stt_plugin::vad::{VadConfig, SPEECH_ENDED_EVENT_TYPE, SPEECH_STARTED_EVENT_TYPE};
use vynkor_sdk::proto::{
    envelope, ActionResponse, ActionStatus, AudioCodec, Envelope, PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "stt";
const PLUGIN_VERSION: &str = "0.3.0";

fn manifest() -> PluginManifest {
    PluginManifest {
        // `network`: the cloud provider invokes `network`'s gated
        // `http_request`, and T-19 requires callers of a gated action to hold
        // its permission too (matches plugin.json `permissions`; Manifest v2
        // per-action model).
        // `secrets`: the cloud provider resolves its API key through the
        // `secrets` plugin's gated `secret_get` action first (env var is
        // only the fallback), so stt must hold PERMISSION_SECRETS too.
        // `audio_stream`: the listen path receives `AudioStreamChunk` PCM
        // from a mic peer (PERMISSION_AUDIO_STREAM, proto v1.6).
        // `event_publish`: `stt_listen_stop` publishes the transcript as an
        // `stt_text` event (PERMISSION_EVENT_PUBLISH).
        permissions: vec![
            "PERMISSION_NETWORK".into(),
            "PERMISSION_SECRETS".into(),
            "PERMISSION_AUDIO_STREAM".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: vec![
            "stt_transcribe".to_string(),
            "stt_models".to_string(),
            "stt_listen_start".to_string(),
            "stt_listen_stop".to_string(),
        ],
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
        "stt_transcribe" => match handler::handle_stt_transcribe(client, &req.params_json).await {
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
        "stt_models" => match handler::handle_stt_models(client, &req.params_json).await {
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
        "stt_listen_start" => {
            match handler::handle_stt_listen_start(client, &req.params_json).await {
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
            }
        }
        "stt_listen_stop" => {
            match handler::handle_stt_listen_stop(client, &req.params_json).await {
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
            }
        }
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

    let vad_cfg = VadConfig::from_env();
    if vad_cfg.enabled {
        println!(
            "[{PLUGIN_ID}] voice-activity detection on (rms {}, silence {} ms, min speech {} ms)",
            vad_cfg.rms_threshold, vad_cfg.silence_ms, vad_cfg.min_speech_ms
        );
    }

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
                // stt declares no event subscriptions; ack defensively so
                // the kernel doesn't retry anything unexpectedly delivered.
                let _ = client.ack_event(&event.event_id).await;
            }
            Some(envelope::Payload::AudioStreamChunk(chunk)) => {
                // D-12 listen path: accumulate PCM from a mic peer; the
                // transcript is produced by the stt_listen_stop action.
                // With the VAD enabled, speech boundaries also go out as
                // best-effort events for orchestrators like `daemon`.
                if chunk.codec != AudioCodec::PcmS16le as i32 {
                    println!(
                        "[{PLUGIN_ID}] ignoring audio stream {} codec {} (expected PCM_S16LE)",
                        chunk.stream_id, chunk.codec
                    );
                    continue;
                }
                match stt_plugin::listen::push(
                    chunk.stream_id,
                    chunk.sample_rate,
                    chunk.channels.max(1) as u16,
                    &chunk.data,
                    &vad_cfg,
                ) {
                    Ok(outcome) => publish_vad(&mut client, chunk.stream_id, outcome).await,
                    Err(e) => println!("[{PLUGIN_ID}] {e}"),
                }
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

async fn publish_vad(client: &mut VynkorClient, stream_id: u32, outcome: stt_plugin::listen::ChunkVad) {
    if outcome.speech_started {
        let payload = serde_json::json!({ "stream_id": stream_id });
        if let Err(e) = client
            .publish_event(SPEECH_STARTED_EVENT_TYPE, payload.to_string().as_bytes(), 5000)
            .await
        {
            eprintln!("[{PLUGIN_ID}] failed to publish {SPEECH_STARTED_EVENT_TYPE}: {e}");
        }
    }
    if let Some(speech_ms) = outcome.speech_ended_ms {
        let payload =
            serde_json::json!({ "stream_id": stream_id, "speech_ms": speech_ms });
        if let Err(e) = client
            .publish_event(SPEECH_ENDED_EVENT_TYPE, payload.to_string().as_bytes(), 5000)
            .await
        {
            eprintln!("[{PLUGIN_ID}] failed to publish {SPEECH_ENDED_EVENT_TYPE}: {e}");
        }
    }
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
