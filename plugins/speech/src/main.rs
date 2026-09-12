//! `speech` plugin (MRG-01) — `tts` and `stt` merged into one process, one
//! registration, one config surface. The two engines live in path-dep
//! sub-crates (`synth-engine` = the tts sources, `listen-engine` = the stt
//! sources) copied verbatim: every action name (`tts_*` / `stt_*`), event
//! type (`stt_text`, speech boundaries), env var (`TTS_PLUGIN_*`,
//! `STT_PLUGIN_*`) and wire shape is unchanged, so callers (daemon,
//! webclient) and operators need zero edits — only the binary/plugin_id
//! differs.
//!
//! Loop = stt's superset: it owns inbound `AudioStreamChunk` PCM (the listen
//! path) while `tts_speak`/`tts_speak_stream` send chunks out through the
//! same client. The standalone `tts`/`stt` plugins remain shipped; run
//! either this or both singles — never this alongside them on one machine's
//! audio devices without deciding who owns the mic.

use listen_engine as listen;
use synth_engine as synth;

use vynkor_sdk::proto::{
    envelope, ActionResponse, ActionStatus, AudioCodec, Envelope, PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "speech";
const PLUGIN_VERSION: &str = "0.1.0";

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec![
            "PERMISSION_NETWORK".into(),
            "PERMISSION_SECRETS".into(),
            "PERMISSION_AUDIO_STREAM".into(),
            "PERMISSION_IPC_SEND".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: vec![
            "tts_synthesize".to_string(),
            "tts_voices".to_string(),
            "tts_speak".to_string(),
            "tts_speak_stream".to_string(),
            "stt_transcribe".to_string(),
            "stt_models".to_string(),
            "stt_listen_start".to_string(),
            "stt_listen_stop".to_string(),
            "status".to_string(),
        ],
        ipc_targets: ipc_targets(),
        ..Default::default()
    }
}

/// Outbound streaming allowlist — same env var and semantics as the tts
/// plugin's (`tts_speak` targets).
fn ipc_targets() -> Vec<String> {
    std::env::var("TTS_PLUGIN_IPC_TARGETS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
fn start_instant() -> std::time::Instant { *START.get_or_init(std::time::Instant::now) }
fn status_payload() -> Vec<u8> {
    let uptime_ms = start_instant().elapsed().as_millis() as u64;
    serde_json::to_vec(&serde_json::json!({
        "version": PLUGIN_VERSION,
        "uptime_ms": uptime_ms,
        "engine_ready": true,
        "last_error": null,
        "counters": {}
    })).unwrap()
}

fn ok(action_id: String, data_json: Vec<u8>) -> ActionResponse {
    ActionResponse { action_id, status: ActionStatus::ActionOk as i32, data_json, error: String::new() }
}

fn err(action_id: String, error: String) -> ActionResponse {
    ActionResponse { action_id, status: ActionStatus::ActionError as i32, data_json: Vec::new(), error }
}

async fn dispatch(client: &mut VynkorClient, req: vynkor_sdk::proto::ActionRequest) -> Envelope {
    eprintln!("[speech] dispatch {} ({} bytes params)", req.action, req.params_json.len());
    let reply = match req.action.as_str() {
        // ---- synthesis (the old `tts`) ---------------------------------
        "tts_synthesize" => {
            synth::handler::handle_tts_synthesize(client, &req.params_json).await
        }
        "tts_voices" => synth::handler::handle_tts_voices(client, &req.params_json).await,
        "tts_speak" => synth::handler::handle_tts_speak(client, &req.params_json).await,
        "tts_speak_stream" => {
            synth::handler::handle_tts_speak_stream(client, &req.params_json).await
        }
        // ---- listening (the old `stt`) ---------------------------------
        "stt_transcribe" => {
            listen::handler::handle_stt_transcribe(client, &req.params_json).await
        }
        "stt_models" => listen::handler::handle_stt_models(client, &req.params_json).await,
        "stt_listen_start" => {
            listen::handler::handle_stt_listen_start(client, &req.params_json).await
        }
        "stt_listen_stop" => {
            listen::handler::handle_stt_listen_stop(client, &req.params_json).await
        }
        "status" => Ok(status_payload()),
        other => return Envelope {
            payload: Some(envelope::Payload::ActionResponse(err(
                req.action_id,
                format!("unknown action: {other}"),
            ))),
            ..Default::default()
        },
    };
    let reply = match reply {
        Ok(data_json) => { eprintln!("[speech] dispatch done ok"); ok(req.action_id, data_json) }
        Err(error) => { eprintln!("[speech] dispatch err: {error}"); err(req.action_id, error) }
    };
    Envelope { payload: Some(envelope::Payload::ActionResponse(reply)), ..Default::default() }
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
    println!("[{PLUGIN_ID}] ipc_targets from env: {:?}", ipc_targets());

    let vad_cfg = listen::vad::VadConfig::from_env();
    if vad_cfg.enabled {
        println!("[{PLUGIN_ID}] VAD on");
    }

    loop {
        let env = match client.recv().await {
            Ok(env) => env,
            Err(_) => break,
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
                let _ = client.ack_event(&event.event_id).await;
            }
            Some(envelope::Payload::AudioStreamChunk(chunk)) => {
                // D-12 listen leg: mic PCM in; transcript on stt_listen_stop.
                if chunk.codec != AudioCodec::PcmS16le as i32 {
                    println!(
                        "[{PLUGIN_ID}] ignoring audio stream {} codec {}",
                        chunk.stream_id, chunk.codec
                    );
                    continue;
                }
                match listen::listen::push(
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
                let resp = dispatch(&mut client, req).await;
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

async fn publish_vad(
    client: &mut VynkorClient,
    stream_id: u32,
    outcome: listen::listen::ChunkVad,
) {
    use listen::vad::{SPEECH_ENDED_EVENT_TYPE, SPEECH_STARTED_EVENT_TYPE};
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
        let payload = serde_json::json!({ "stream_id": stream_id, "speech_ms": speech_ms });
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
    let secret = std::env::var("VYN_JWT_SECRET").ok().filter(|s| !s.is_empty());
    let client = match secret {
        Some(s) => VynkorClient::connect_with_secret(&socket_path, s.as_bytes()).await?,
        None => VynkorClient::connect(&socket_path).await?,
    };
    serve(client).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_payload_reports_inf07_shape() {
        let v: serde_json::Value = serde_json::from_slice(&status_payload()).unwrap();
        assert_eq!(v["version"], "0.1.0");
        assert_eq!(v["engine_ready"], true);
        assert_eq!(v["last_error"], serde_json::Value::Null);
        assert_eq!(v["counters"], serde_json::json!({}));
        assert!(v["uptime_ms"].as_u64().is_some());
    }
}
