//! Glue: validate a request, dispatch to the right provider, and map the
//! result back to `stt`'s normalized shape.
//!
//!   - `sherpa` (local): transcribe in-process via sherpa-onnx — no HTTP,
//!     no `network` hop.
//!   - `openai` (cloud): build the multipart audio upload, send it through
//!     `network`'s `http_request` action, parse the transcript — same flow
//!     as `ai`'s `chat_completion` and `tts`'s cloud handlers.

use vynkor_sdk::VynkorClient;

use crate::provider::{openai::OpenAiProvider, ModelInfo, Provider, TranscriptResult};
use crate::request::{self, Provider as ProviderKind, TranscribeParams};

/// `network`'s `http_request` response shape (see
/// `plugins/network/src/handler.rs::HttpResponseJson`) — only the fields
/// `stt` needs to decode.
#[derive(serde::Deserialize)]
struct NetworkHttpResponse {
    status: u16,
    body: String,
    body_encoding: String,
}

/// Handle one `stt_transcribe` action end to end. Returns the JSON to
/// place in `ActionResponse.data_json` on success, or a human-readable
/// error (never containing a resolved API key) on failure.
pub async fn handle_stt_transcribe(
    client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let mut params = request::parse_request(params_json)?;
    if params.provider == request::Provider::Sherpa && params.format == request::AudioFormat::Ogg {
        let wav_bytes = tokio::task::spawn_blocking({
            let ogg = params.audio.clone();
            move || {
                let dir = std::env::temp_dir();
                let ogg_path = dir.join(format!("stt_in_{}.ogg", std::process::id()));
                let wav_path = dir.join(format!("stt_out_{}.wav", std::process::id()));
                std::fs::write(&ogg_path, &ogg).map_err(|e| format!("failed to write temp ogg: {e}"))?;
                let status = std::process::Command::new("ffmpeg")
                    .args([
                        "-y",
                        "-i",
                        ogg_path.to_str().unwrap(),
                        "-ar",
                        "16000",
                        "-ac",
                        "1",
                        "-f",
                        "wav",
                        wav_path.to_str().unwrap(),
                    ])
                    .output()
                    .map_err(|e| format!("ffmpeg not found or failed to start: {e}"))?;
                if !status.status.success() {
                    return Err(format!(
                        "ffmpeg failed: {}",
                        String::from_utf8_lossy(&status.stderr)
                    ));
                }
                let wav = std::fs::read(&wav_path).map_err(|e| format!("failed to read wav: {e}"))?;
                let _ = std::fs::remove_file(&ogg_path);
                let _ = std::fs::remove_file(&wav_path);
                Ok::<Vec<u8>, String>(wav)
            }
        })
        .await
        .map_err(|e| format!("ffmpeg task failed: {e}"))??;
        params.audio = wav_bytes;
        params.format = request::AudioFormat::Wav;
    }

    let result = match params.provider {
        ProviderKind::Sherpa => {
            let p = params.clone();
            tokio::task::spawn_blocking(move || crate::provider::sherpa::transcribe(&p))
                .await
                .map_err(|e| format!("sherpa transcribe task failed: {e}"))??
        }
        ProviderKind::OpenAi => transcribe_cloud(client, &params).await?,
    };

    serde_json::to_vec(&result).map_err(|e| format!("failed to encode response: {e}"))
}

/// Handle one `stt_models` action: list the models the provider exposes.
pub async fn handle_stt_models(
    _client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let provider = request::parse_models_request(params_json)?;
    let models: Vec<ModelInfo> = match provider {
        ProviderKind::Sherpa => tokio::task::spawn_blocking(crate::provider::sherpa::models)
            .await
            .map_err(|e| format!("sherpa models task failed: {e}"))??,
        ProviderKind::OpenAi => request::OPENAI_MODELS
            .iter()
            .map(|m| ModelInfo {
                id: m.to_string(),
                name: m.to_string(),
            })
            .collect(),
    };
    serde_json::to_vec(&models).map_err(|e| format!("failed to encode response: {e}"))
}

async fn transcribe_cloud(
    client: &mut VynkorClient,
    params: &TranscribeParams,
) -> Result<TranscriptResult, String> {
    let allowed = request::parse_allowed_key_envs(
        &std::env::var(request::ALLOWED_KEY_ENVS_ENV).unwrap_or_default(),
    );
    if !request::is_allowed_key_env(&params.api_key_env, &allowed) {
        return Err(format!(
            "api_key_env '{}' is not in the operator's {} allowlist",
            params.api_key_env,
            request::ALLOWED_KEY_ENVS_ENV
        ));
    }

    let api_key = crate::key_resolve::resolve_api_key(client, &params.api_key_env).await?;

    let provider: &dyn Provider = &OpenAiProvider;
    let http_req = provider.build_http_request(params, &api_key);
    let http_req_json = serde_json::to_vec(&http_req)
        .map_err(|e| format!("failed to encode outbound http request: {e}"))?;

    let action_timeout = request::NETWORK_MAX_TIMEOUT_MS.min(params.timeout_ms) as u32;
    let action_resp = client
        .send_action("http_request", &http_req_json, action_timeout)
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

    provider.parse_response(&body_bytes, params)
}

/// Convenience for tests: normalize a cloud provider's transcript body
/// without a live `network` hop. Not part of the plugin's public interface.
pub fn parse_cloud_body(
    body: &[u8],
    params: &TranscribeParams,
) -> Result<TranscriptResult, String> {
    let provider: &dyn Provider = &OpenAiProvider;
    provider.parse_response(body, params)
}

/// Event type (pre-namespacing) the listen path publishes a transcript to.
/// The kernel namespaces it as `plugin.stt.stt_text` for subscribers.
pub const TEXT_EVENT_TYPE: &str = "stt_text";

/// Handle one `stt_listen_start` action: open an accumulation buffer for an
/// inbound PCM audio stream. The mic-side peer then sends `AudioStreamChunk`
/// envelopes (codec `PCM_S16LE`) addressed to `stt`.
pub async fn handle_stt_listen_start(
    _client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let params = request::parse_listen_start_request(params_json)?;
    crate::listen::start(
        params.stream_id,
        params.sample_rate_hz,
        params.num_channels,
        params.language,
    )?;
    Ok(serde_json::json!({
        "stream_id": params.stream_id,
        "status": "listening",
    })
    .to_string()
    .into_bytes())
}

/// Handle one `stt_listen_stop` action: transcribe the accumulated PCM for
/// the stream (local sherpa only), publish the transcript as an
/// `stt_text` event, and return it in the action response.
pub async fn handle_stt_listen_stop(
    client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let params = request::parse_listen_stop_request(params_json)?;
    let mut stream = crate::listen::take(params.stream_id)?;
    if stream.is_empty() {
        return Err(format!(
            "listen stream {} has no audio buffered",
            params.stream_id
        ));
    }
    let pcm = stream.take_pcm();
    let rate = stream.rate_hz;
    let lang = stream.language.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::provider::sherpa::transcribe_pcm(&pcm, rate, lang.as_deref())
    })
    .await
    .map_err(|e| format!("sherpa transcribe task failed: {e}"))??;

    let event = serde_json::json!({
        "stream_id": params.stream_id,
        "text": result.text,
        "language": result.language,
        "duration_seconds": result.duration_seconds,
        "model": result.model,
    });
    client
        .publish_event(TEXT_EVENT_TYPE, event.to_string().as_bytes(), 5000)
        .await
        .map_err(|e| format!("failed to publish stt_text event: {e}"))?;

    Ok(event.to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_params() -> TranscribeParams {
        TranscribeParams {
            provider: ProviderKind::OpenAi,
            audio: vec![0x52, 0x49, 0x46, 0x46],
            format: crate::request::AudioFormat::Wav,
            sample_rate_hz: 0,
            num_channels: 1,
            language: None,
            prompt: None,
            temperature: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            base_url: None,
            model: None,
            timeout_ms: 30_000,
        }
    }

    #[test]
    fn parse_cloud_body_parses_openai_json() {
        let result = parse_cloud_body(br#"{"text": "Hello world."}"#, &openai_params()).unwrap();
        assert_eq!(result.text, "Hello world.");
        assert_eq!(result.model, "whisper-1");
    }

    #[test]
    fn parse_cloud_body_propagates_provider_errors() {
        let err = parse_cloud_body(b"not json", &openai_params()).unwrap_err();
        assert!(err.contains("malformed"), "error was: {err}");
    }

    #[test]
    fn parse_cloud_body_rejects_empty_transcript() {
        let err = parse_cloud_body(br#"{"text": ""}"#, &openai_params()).unwrap_err();
        assert!(err.contains("empty transcript"), "error was: {err}");
    }
}
