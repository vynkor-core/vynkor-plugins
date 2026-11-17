//! ElevenLabs TTS adapter (POST /v1/text-to-speech/{voice_id}, xi-api-key).

use std::collections::HashMap;

use crate::provider::{encode_base64, AudioResult, HttpRequestJson, Provider};
use crate::request::{
    AudioFormat, SynthesizeParams, DEFAULT_ELEVENLABS_BASE_URL, DEFAULT_ELEVENLABS_MODEL,
    NETWORK_MAX_TIMEOUT_MS,
};

pub struct ElevenLabsProvider;

impl Provider for ElevenLabsProvider {
    fn build_http_request(&self, params: &SynthesizeParams, api_key: &str) -> HttpRequestJson {
        let base_url = params
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_ELEVENLABS_BASE_URL.to_string());
        // ElevenLabs has no wav output; request.rs already restricts the
        // format enum to mp3|pcm|ulaw for this provider.
        let output_format = match params.format {
            AudioFormat::Mp3 => "mp3_44100_128",
            AudioFormat::Pcm => "pcm_24000",
            AudioFormat::Ulaw => "ulaw_8000",
            other => unreachable!("elevenlabs rejects {} at parse time", other.as_str()),
        };
        let body = serde_json::json!({
            "text": params.text,
            "model_id": params.model.clone().unwrap_or_else(|| DEFAULT_ELEVENLABS_MODEL.to_string()),
            "voice_settings": {
                "stability": 0.5,
                "similarity_boost": 0.75,
                "style": 0.0,
                "use_speaker_boost": true,
            },
        })
        .to_string();

        let mut headers = HashMap::new();
        headers.insert("xi-api-key".to_string(), api_key.to_string());
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        HttpRequestJson {
            method: "POST",
            url: format!(
                "{base_url}/v1/text-to-speech/{}?output_format={output_format}",
                params.voice
            ),
            headers,
            body,
            timeout_ms: params.timeout_ms.min(NETWORK_MAX_TIMEOUT_MS),
        }
    }

    fn parse_response(&self, body: &[u8], format: AudioFormat) -> Result<AudioResult, String> {
        let (sample_rate_hz, num_channels, duration_seconds) = match format {
            // pcm_24000: 24 kHz, 16-bit, mono.
            AudioFormat::Pcm => (24000, 1, body.len() as f32 / (24000 * 2) as f32),
            // ulaw_8000: 8 kHz, 8-bit mu-law, mono (1 byte per sample).
            AudioFormat::Ulaw => (8000, 1, body.len() as f32 / 8000.0),
            _ => (0, 0, 0.0),
        };
        Ok(AudioResult {
            format: format.as_str().to_string(),
            sample_rate_hz,
            num_channels,
            duration_seconds,
            audio_base64: encode_base64(body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Provider as P;

    fn params() -> SynthesizeParams {
        SynthesizeParams {
            provider: P::ElevenLabs,
            text: "hello".to_string(),
            voice: "21m00Tcm4TlvDq8ikWAM".to_string(),
            api_key_env: "ELEVENLABS_API_KEY".to_string(),
            base_url: None,
            model: None,
            format: AudioFormat::Mp3,
            speed: 1.0,
            timeout_ms: 30_000,
        }
    }

    #[test]
    fn build_http_request_shape() {
        let req = ElevenLabsProvider.build_http_request(&params(), "sk-11labs");
        assert_eq!(req.method, "POST");
        assert_eq!(
            req.url,
            "https://api.elevenlabs.io/v1/text-to-speech/21m00Tcm4TlvDq8ikWAM?output_format=mp3_44100_128"
        );
        assert_eq!(req.headers["xi-api-key"], "sk-11labs");
        assert!(req.body.contains("\"model_id\":\"eleven_multilingual_v2\""));
    }

    #[test]
    fn build_http_request_pcm_output_format() {
        let mut p = params();
        p.format = AudioFormat::Pcm;
        let req = ElevenLabsProvider.build_http_request(&p, "sk-11labs");
        assert!(req.url.ends_with("output_format=pcm_24000"));
    }

    #[test]
    fn build_http_request_respects_custom_base_and_caps_timeout() {
        let mut p = params();
        p.base_url = Some("http://localhost:3000".to_string());
        p.timeout_ms = 90_000;
        let req = ElevenLabsProvider.build_http_request(&p, "sk-11labs");
        assert!(req.url.starts_with("http://localhost:3000/"));
        assert_eq!(req.timeout_ms, NETWORK_MAX_TIMEOUT_MS);
    }

    #[test]
    fn parse_mp3_passthrough() {
        let body = vec![0x49, 0x44, 0x33, 0x00];
        let result = ElevenLabsProvider
            .parse_response(&body, AudioFormat::Mp3)
            .unwrap();
        assert_eq!(result.format, "mp3");
        assert_eq!(result.sample_rate_hz, 0);
        assert_eq!(result.duration_seconds, 0.0);
    }

    #[test]
    fn parse_pcm_known_rate_and_duration() {
        let body = vec![0u8; 48_000];
        let result = ElevenLabsProvider
            .parse_response(&body, AudioFormat::Pcm)
            .unwrap();
        assert_eq!(result.format, "pcm");
        assert_eq!(result.sample_rate_hz, 24000);
        assert_eq!(result.num_channels, 1);
        assert_eq!(result.duration_seconds, 1.0);
    }

    #[test]
    fn build_http_request_ulaw_output_format() {
        let mut p = params();
        p.format = AudioFormat::Ulaw;
        let req = ElevenLabsProvider.build_http_request(&p, "sk-11labs");
        assert!(req.url.ends_with("output_format=ulaw_8000"));
    }

    #[test]
    fn parse_ulaw_reports_8khz_rate() {
        // 8000 Hz mono 8-bit mu-law -> 8000 bytes = 1.0 s
        let body = vec![0xFFu8; 8000];
        let result = ElevenLabsProvider
            .parse_response(&body, AudioFormat::Ulaw)
            .unwrap();
        assert_eq!(result.format, "ulaw");
        assert_eq!(result.sample_rate_hz, 8000);
        assert_eq!(result.num_channels, 1);
        assert_eq!(result.duration_seconds, 1.0);
    }
}
