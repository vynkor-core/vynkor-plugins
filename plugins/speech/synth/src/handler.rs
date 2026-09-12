//! Glue: validate a request, dispatch to the right provider, and map the
//! result back to `tts`'s normalized shape.
//!
//!   - `sherpa` (local): synthesize in-process via sherpa-onnx — no HTTP,
//!     no `network` hop.
//!   - `openai` / `elevenlabs` (cloud): build the provider HTTP request,
//!     send it through `network`'s `http_request` action, parse the audio
//!     body — same flow as `ai`'s `chat_completion` handler.

use vynkor_sdk::proto::{AudioCodec, AudioStreamChunk};
use vynkor_sdk::VynkorClient;

use crate::provider::{
    elevenlabs::ElevenLabsProvider, openai::OpenAiProvider, opus, AudioResult, Provider, VoiceInfo,
};
use crate::request::{
    self, AudioFormat, Provider as ProviderKind, SynthesizeParams, OPENAI_VOICES,
};

/// `network`'s `http_request` response shape (see
/// `plugins/network/src/handler.rs::HttpResponseJson`) — only the fields
/// `tts` needs to decode.
#[derive(serde::Deserialize)]
struct NetworkHttpResponse {
    status: u16,
    body: String,
    body_encoding: String,
}

/// Handle one `tts_synthesize` action end to end. Returns the JSON to
/// place in `ActionResponse.data_json` on success, or a human-readable
/// error (never containing a resolved API key) on failure.
pub async fn handle_tts_synthesize(
    client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let params = request::parse_request(params_json)?;

    let result = match params.provider {
        ProviderKind::Sherpa => {
            let p = params.clone();
            eprintln!("[tts] entering sherpa synthesize (blocking)");
            let r = tokio::task::spawn_blocking(move || crate::provider::sherpa::synthesize(&p))
                .await;
            eprintln!("[tts] sherpa synthesize returned");
            r.map_err(|e| format!("sherpa synthesize task failed: {e}"))??
        }
        ProviderKind::OpenAi | ProviderKind::ElevenLabs => {
            synthesize_cloud(client, &params).await?
        }
    };

    serde_json::to_vec(&result).map_err(|e| format!("failed to encode response: {e}"))
}

/// Handle one `tts_voices` action: list the voices the provider exposes.
pub async fn handle_tts_voices(
    _client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let provider = request::parse_voices_request(params_json)?;
    let voices: Vec<VoiceInfo> = match provider {
        ProviderKind::Sherpa => tokio::task::spawn_blocking(crate::provider::sherpa::voices)
            .await
            .map_err(|e| format!("sherpa voices task failed: {e}"))??,
        ProviderKind::OpenAi => OPENAI_VOICES
            .iter()
            .map(|v| VoiceInfo {
                id: v.to_string(),
                name: v.to_string(),
            })
            .collect(),
        ProviderKind::ElevenLabs => {
            return Err(
                "elevenlabs voices are per-account; list them via the ElevenLabs \
                         dashboard or GET /v1/voices with an account key"
                    .to_string(),
            )
        }
    };
    serde_json::to_vec(&voices).map_err(|e| format!("failed to encode response: {e}"))
}

async fn synthesize_cloud(
    client: &mut VynkorClient,
    params: &SynthesizeParams,
) -> Result<AudioResult, String> {
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

    let provider: &dyn Provider = match params.provider {
        ProviderKind::OpenAi => &OpenAiProvider,
        ProviderKind::ElevenLabs => &ElevenLabsProvider,
        ProviderKind::Sherpa => unreachable!("sherpa handled separately"),
    };

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

    provider.parse_response(&body_bytes, params.format)
}

/// Convenience for tests: normalize a cloud provider's audio body without
/// a live `network` hop. Not part of the plugin's public interface.
pub fn parse_cloud_body(
    provider: ProviderKind,
    body: &[u8],
    format: AudioFormat,
) -> Result<AudioResult, String> {
    let provider: &dyn Provider = match provider {
        ProviderKind::OpenAi => &OpenAiProvider,
        ProviderKind::ElevenLabs => &ElevenLabsProvider,
        ProviderKind::Sherpa => return Err("sherpa is not a cloud provider".to_string()),
    };
    provider.parse_response(body, format)
}

/// Handle one `tts_speak` action: synthesize locally (sherpa), encode the
/// PCM as Opus, and stream it as `AudioStreamChunk`s to the `target` peer
/// (e.g. a client speaker plugin). Returns a JSON summary of the stream.
///
/// The chunk stream is fire-and-forget from the kernel's perspective — the
/// kernel routes each `AudioStreamChunk` envelope to `target` like any
/// other message; delivery failure is the caller's to detect (no ack).
pub async fn handle_tts_speak(
    client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let params = request::parse_speak_request(params_json)?;

    let text = params.text.clone();
    let voice = params.voice.clone();
    let speed = params.speed;
    let (samples, model_rate) = tokio::task::spawn_blocking(move || {
        crate::provider::sherpa::synthesize_samples(&text, &voice, speed)
    })
    .await
    .map_err(|e| format!("sherpa synthesize task failed: {e}"))??;
    let sample_rate = if params.sample_rate_hz == 0 {
        model_rate
    } else {
        params.sample_rate_hz
    };
    let config = opus::OpusConfig {
        sample_rate_hz: sample_rate,
        channels: 1,
        bitrate: params.bitrate,
    };

    let resampled: Vec<f32> = if model_rate != sample_rate {
        resample_linear(&samples, model_rate, sample_rate)
    } else {
        samples
    };
    let pcm: Vec<i16> = resampled
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let packets = opus::encode_pcm(&pcm, &config)?;

    let duration_seconds = resampled.len() as f32 / sample_rate.max(1) as f32;
    let total = packets.len();
    let started = std::time::Instant::now();
    for (idx, packet) in packets.into_iter().enumerate() {
        if idx == 0 {
            eprintln!("[tts] time-to-first-audio: {} ms", started.elapsed().as_millis());
        }
        let chunk = AudioStreamChunk {
            stream_id: params.stream_id,
            codec: AudioCodec::Opus as i32,
            sample_rate,
            channels: 1,
            data: packet,
            end_of_stream: idx + 1 == total,
        };
        client
            .send_audio_chunk(&params.target, chunk)
            .await
            .map_err(|e| format!("failed to stream audio chunk to '{}': {e}", params.target))?;
    }

    Ok(serde_json::json!({
        "codec": "opus",
        "stream_id": params.stream_id,
        "target": params.target,
        "sample_rate_hz": sample_rate,
        "num_channels": 1,
        "duration_seconds": duration_seconds,
        "packets": total,
    })
    .to_string()
    .into_bytes())
}

/// Handle one `tts_speak_stream` action (EXI-02): like `tts_speak`, but the
/// text is split into sentences first and each sentence is synthesized,
/// encoded and streamed in turn — the peer starts hearing audio after one
/// phrase instead of after the whole paragraph. Only the final packet of the
/// final sentence carries `end_of_stream`.
pub async fn handle_tts_speak_stream(
    client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let params = request::parse_speak_request(params_json)?;
    let sentences = crate::sentence::split_sentences(&params.text);
    let sentences: Vec<String> = if sentences.is_empty() {
        vec![params.text.clone()]
    } else {
        sentences
    };

    let config = opus::OpusConfig { sample_rate_hz: 0, channels: 1, bitrate: params.bitrate };
    let _ = config; // rebuilt per sentence with the resolved rate

    let total_sentences = sentences.len();
    let mut packets_sent = 0usize;
    let mut duration_seconds = 0f32;
    let mut sample_rate = params.sample_rate_hz;

    for (idx, sentence) in sentences.iter().enumerate() {
        let is_last_sentence = idx + 1 == total_sentences;
        let text = sentence.clone();
        let voice = params.voice.clone();
        let speed = params.speed;
        let (samples, model_rate) =
            tokio::task::spawn_blocking(move || {
                crate::provider::sherpa::synthesize_samples(&text, &voice, speed)
            })
            .await
            .map_err(|e| format!("sherpa synthesize task failed: {e}"))??;
        sample_rate = if params.sample_rate_hz == 0 { model_rate } else { params.sample_rate_hz };
        let opus_config = opus::OpusConfig {
            sample_rate_hz: sample_rate,
            channels: 1,
            bitrate: params.bitrate,
        };
        let resampled: Vec<f32> = if model_rate != sample_rate {
            resample_linear(&samples, model_rate, sample_rate)
        } else {
            samples
        };
        let pcm: Vec<i16> = resampled.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
        let packets = opus::encode_pcm(&pcm, &opus_config)?;
        duration_seconds += resampled.len() as f32 / sample_rate.max(1) as f32;

        let count = packets.len();
        let sentence_started = std::time::Instant::now();
        for (pi, packet) in packets.into_iter().enumerate() {
            if pi == 0 && idx == 0 {
                eprintln!(
                    "[tts] time-to-first-audio (stream): {} ms (first of {total_sentences} sentences)",
                    sentence_started.elapsed().as_millis()
                );
            }
            let chunk = AudioStreamChunk {
                stream_id: params.stream_id,
                codec: AudioCodec::Opus as i32,
                sample_rate,
                channels: 1,
                data: packet,
                end_of_stream: is_last_sentence && pi + 1 == count,
            };
            client
                .send_audio_chunk(&params.target, chunk)
                .await
                .map_err(|e| {
                    format!("failed to stream audio chunk to '{}': {e}", params.target)
                })?;
        }
        packets_sent += count;
    }

    Ok(serde_json::json!({
        "codec": "opus",
        "stream_id": params.stream_id,
        "target": params.target,
        "sample_rate_hz": sample_rate,
        "num_channels": 1,
        "duration_seconds": duration_seconds,
        "packets": packets_sent,
        "sentences": total_sentences,
    })
    .to_string()
    .into_bytes())
}

fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || from_rate == 0 || to_rate == 0 {
        return input.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = src_pos - idx as f64;
        if idx + 1 < input.len() {
            let a = input[idx] as f64;
            let b = input[idx + 1] as f64;
            out.push((a * (1.0 - frac) + b * frac) as f32);
        } else if idx < input.len() {
            out.push(input[idx]);
        } else {
            out.push(0.0);
        }
    }
    out
}
