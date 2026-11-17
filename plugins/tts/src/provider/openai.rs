//! OpenAI TTS adapter (POST /v1/audio/speech, Bearer auth).

use std::collections::HashMap;

use crate::provider::{
    encode_base64, wav_duration_seconds, wav_info, AudioResult, HttpRequestJson, Provider,
};
use crate::request::{
    AudioFormat, SynthesizeParams, DEFAULT_OPENAI_BASE_URL, DEFAULT_OPENAI_MODEL,
    NETWORK_MAX_TIMEOUT_MS,
};

pub struct OpenAiProvider;

impl Provider for OpenAiProvider {
    fn build_http_request(&self, params: &SynthesizeParams, api_key: &str) -> HttpRequestJson {
        let base_url = params
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
        let format = match params.format {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Wav => "wav",
            AudioFormat::Pcm => "pcm",
            AudioFormat::Opus => "opus",
            AudioFormat::Aac => "aac",
            AudioFormat::Flac => "flac",
            AudioFormat::Ulaw => unreachable!("openai rejects ulaw at parse time"),
        };
        let body = serde_json::json!({
            "model": params.model.clone().unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string()),
            "input": params.text,
            "voice": params.voice,
            "response_format": format,
            "speed": params.speed,
        })
        .to_string();

        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        HttpRequestJson {
            method: "POST",
            url: format!("{base_url}/audio/speech"),
            headers,
            body,
            timeout_ms: params.timeout_ms.min(NETWORK_MAX_TIMEOUT_MS),
        }
    }

    fn parse_response(&self, body: &[u8], format: AudioFormat) -> Result<AudioResult, String> {
        let (sample_rate_hz, num_channels, duration_seconds) = match format {
            // Raw PCM: OpenAI documents pcm as 24 kHz, 16-bit, mono.
            AudioFormat::Pcm => (24000, 1, body.len() as f32 / (24000 * 2) as f32),
            AudioFormat::Wav => {
                let (rate, channels) = wav_info(body).unwrap_or((0, 0));
                let duration = wav_duration_seconds(body).unwrap_or(0.0);
                (rate, channels as u8, duration)
            }
            // No reliable header in the container for MP3/opus/aac/flac here.
            AudioFormat::Mp3 | AudioFormat::Opus | AudioFormat::Aac | AudioFormat::Flac => {
                (0, 0, 0.0)
            }
            AudioFormat::Ulaw => unreachable!("openai rejects ulaw at parse time"),
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
            provider: P::OpenAi,
            text: "hello".to_string(),
            voice: "alloy".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            base_url: None,
            model: None,
            format: AudioFormat::Mp3,
            speed: 1.0,
            timeout_ms: 30_000,
        }
    }

    #[test]
    fn build_http_request_shape() {
        let req = OpenAiProvider.build_http_request(&params(), "sk-test");
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://api.openai.com/v1/audio/speech");
        assert_eq!(req.headers["Authorization"], "Bearer sk-test");
        assert!(req.body.contains("\"voice\":\"alloy\""));
        assert!(req.body.contains("\"response_format\":\"mp3\""));
        assert_eq!(req.timeout_ms, 30_000);
    }

    #[test]
    fn build_http_request_respects_base_url_and_caps_timeout() {
        let mut p = params();
        p.base_url = Some("http://localhost:8080/v1".to_string());
        p.timeout_ms = 90_000;
        let req = OpenAiProvider.build_http_request(&p, "sk-test");
        assert_eq!(req.url, "http://localhost:8080/v1/audio/speech");
        assert_eq!(req.timeout_ms, NETWORK_MAX_TIMEOUT_MS);
    }

    #[test]
    fn build_http_request_uses_pcm_format() {
        let mut p = params();
        p.format = AudioFormat::Pcm;
        let req = OpenAiProvider.build_http_request(&p, "sk-test");
        assert!(req.body.contains("\"response_format\":\"pcm\""));
    }

    #[test]
    fn parse_mp3_passthrough() {
        let body = vec![0x49, 0x44, 0x33, 0x04, 0x00];
        let result = OpenAiProvider
            .parse_response(&body, AudioFormat::Mp3)
            .unwrap();
        assert_eq!(result.format, "mp3");
        assert_eq!(result.sample_rate_hz, 0);
        assert_eq!(result.num_channels, 0);
        assert!(result.audio_base64.starts_with("SUQz"));
    }

    #[test]
    fn parse_wav_recovers_metadata() {
        let wav = crate::provider::f32_to_wav(&[0.0; 48000], 24000, 1);
        let result = OpenAiProvider
            .parse_response(&wav, AudioFormat::Wav)
            .unwrap();
        assert_eq!(result.format, "wav");
        assert_eq!(result.sample_rate_hz, 24000);
        assert_eq!(result.num_channels, 1);
        assert_eq!(result.duration_seconds, 2.0);
    }

    #[test]
    fn parse_pcm_known_rate_and_duration() {
        // 24000 Hz mono 16-bit -> 96000 bytes = 2.0 s
        let body = vec![0u8; 96_000];
        let result = OpenAiProvider
            .parse_response(&body, AudioFormat::Pcm)
            .unwrap();
        assert_eq!(result.format, "pcm");
        assert_eq!(result.sample_rate_hz, 24000);
        assert_eq!(result.duration_seconds, 2.0);
    }

    #[test]
    fn build_http_request_passes_new_formats_through() {
        for (format, value) in [
            (AudioFormat::Opus, "opus"),
            (AudioFormat::Aac, "aac"),
            (AudioFormat::Flac, "flac"),
        ] {
            let mut p = params();
            p.format = format;
            let req = OpenAiProvider.build_http_request(&p, "sk-test");
            assert!(
                req.body
                    .contains(&format!("\"response_format\":\"{value}\"")),
                "body was: {}",
                req.body
            );
        }
    }

    #[test]
    fn parse_new_container_formats_report_zero_metadata() {
        for format in [AudioFormat::Opus, AudioFormat::Aac, AudioFormat::Flac] {
            let body = vec![0xFF, 0x00, 0x01, 0x02];
            let result = OpenAiProvider.parse_response(&body, format).unwrap();
            assert_eq!(result.format, format.as_str());
            assert_eq!(result.sample_rate_hz, 0);
            assert_eq!(result.num_channels, 0);
            assert_eq!(result.duration_seconds, 0.0);
        }
    }
}
