//! OpenAI STT adapter (POST /v1/audio/transcriptions, Bearer auth).
//!
//! The request is a `multipart/form-data` upload of the audio file. `stt`
//! builds that body itself (no form library) and sends it through
//! `network`'s `http_request` action as `body_base64`, since `network`
//! ships `body` as UTF-8 text only and binary bytes would be mangled.

use std::collections::HashMap;

use crate::provider::{
    encode_base64, wav_duration_seconds, HttpRequestJson, Provider, TranscriptResult,
};
use crate::request::{
    AudioFormat, TranscribeParams, DEFAULT_OPENAI_BASE_URL, DEFAULT_OPENAI_MODEL,
    NETWORK_MAX_TIMEOUT_MS,
};

pub struct OpenAiProvider;

/// Content type of the uploaded audio file, per input format. Raw `pcm` is
/// rejected for `openai` at parse time (request.rs), so it's unreachable
/// here.
fn file_content_type(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Wav => "audio/wav",
        AudioFormat::Mp3 => "audio/mpeg",
        AudioFormat::Ogg => "audio/ogg",
        AudioFormat::Pcm => unreachable!("openai rejects pcm at parse time"),
    }
}

/// Build a `multipart/form-data` request body: a set of text fields plus
/// one binary file part. The boundary is caller-supplied so tests can pin
/// it.
fn build_multipart(
    boundary: &str,
    fields: &[(&str, String)],
    file_name: &str,
    file_content_type: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {file_content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// A boundary unlikely to collide with audio bytes: process id + monotonic
/// nanos. Collision with file content is harmless in practice (the boundary
/// just needs to not appear in the body), and this is unique per process.
fn fresh_boundary() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("stt-{}-{nanos}", std::process::id())
}

impl Provider for OpenAiProvider {
    fn build_http_request(&self, params: &TranscribeParams, api_key: &str) -> HttpRequestJson {
        let base_url = params
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
        let model = params
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string());
        let boundary = fresh_boundary();
        let file_name = format!("audio.{}", params.format.as_str());

        let mut fields: Vec<(&str, String)> = vec![("model", model)];
        if let Some(lang) = &params.language {
            fields.push(("language", lang.clone()));
        }
        if let Some(prompt) = &params.prompt {
            fields.push(("prompt", prompt.clone()));
        }
        if let Some(temp) = params.temperature {
            fields.push(("temperature", temp.to_string()));
        }

        let body = build_multipart(
            &boundary,
            &fields,
            &file_name,
            file_content_type(params.format),
            &params.audio,
        );

        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        headers.insert(
            "Content-Type".to_string(),
            format!("multipart/form-data; boundary={boundary}"),
        );

        HttpRequestJson {
            method: "POST",
            url: format!("{base_url}/audio/transcriptions"),
            headers,
            body: None,
            body_base64: Some(encode_base64(&body)),
            timeout_ms: params.timeout_ms.min(NETWORK_MAX_TIMEOUT_MS),
        }
    }

    fn parse_response(
        &self,
        body: &[u8],
        params: &TranscribeParams,
    ) -> Result<TranscriptResult, String> {
        // The default `json` response_format returns {"text": "..."}.
        #[derive(serde::Deserialize)]
        struct OpenAiJson {
            text: String,
        }
        let parsed: OpenAiJson = serde_json::from_slice(body)
            .map_err(|e| format!("malformed openai transcription response: {e}"))?;
        let text = parsed.text.trim().to_string();
        if text.is_empty() {
            return Err("openai returned an empty transcript".to_string());
        }

        // Duration is only derivable here for wav uploads (header carries
        // it); mp3/ogg round-trip as 0.
        let duration_seconds = match params.format {
            AudioFormat::Wav => wav_duration_seconds(&params.audio).unwrap_or(0.0),
            _ => 0.0,
        };

        Ok(TranscriptResult {
            text,
            language: params.language.clone().unwrap_or_default(),
            duration_seconds,
            model: params
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Provider as P;

    fn params(format: AudioFormat) -> TranscribeParams {
        TranscribeParams {
            provider: P::OpenAi,
            audio: vec![0x52, 0x49, 0x46, 0x46], // "RIFF" placeholder
            format,
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

    fn decoded_body(req: &HttpRequestJson) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(req.body_base64.as_deref().unwrap())
            .unwrap()
    }

    #[test]
    fn build_http_request_shape() {
        let req = OpenAiProvider.build_http_request(&params(AudioFormat::Wav), "sk-test");
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://api.openai.com/v1/audio/transcriptions");
        assert_eq!(req.headers["Authorization"], "Bearer sk-test");
        assert!(req.headers["Content-Type"].starts_with("multipart/form-data; boundary="));
        assert!(req.body.is_none(), "binary body must use body_base64");
        assert!(req.body_base64.is_some());
        assert_eq!(req.timeout_ms, 30_000);
    }

    #[test]
    fn build_http_request_multipart_contains_file_and_model() {
        let req = OpenAiProvider.build_http_request(&params(AudioFormat::Wav), "sk-test");
        let body = decoded_body(&req);
        let boundary = req.headers["Content-Type"]
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap()
            .to_string();

        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains("name=\"model\"\r\n\r\nwhisper-1"),
            "multipart must carry the default model"
        );
        assert!(
            body_str.contains("name=\"file\"; filename=\"audio.wav\""),
            "multipart must carry the file part with a wav filename"
        );
        assert!(body_str.contains("Content-Type: audio/wav"));
        assert!(
            body_str.contains(&format!("--{boundary}--")),
            "closing boundary missing"
        );
    }

    #[test]
    fn build_http_request_includes_optional_fields_when_set() {
        let mut p = params(AudioFormat::Mp3);
        p.language = Some("de".to_string());
        p.prompt = Some("Zahlen, bitte".to_string());
        p.temperature = Some(0.4);
        let req = OpenAiProvider.build_http_request(&p, "sk-test");
        let body = String::from_utf8_lossy(&decoded_body(&req)).to_string();
        assert!(body.contains("name=\"language\"\r\n\r\nde"));
        assert!(body.contains("name=\"prompt\"\r\n\r\nZahlen, bitte"));
        assert!(body.contains("name=\"temperature\"\r\n\r\n0.4"));
        assert!(body.contains("filename=\"audio.mp3\""));
        assert!(body.contains("Content-Type: audio/mpeg"));
    }

    #[test]
    fn build_http_request_respects_base_url_and_caps_timeout() {
        let mut p = params(AudioFormat::Ogg);
        p.base_url = Some("http://localhost:8080/v1".to_string());
        p.timeout_ms = 90_000;
        let req = OpenAiProvider.build_http_request(&p, "sk-test");
        assert_eq!(req.url, "http://localhost:8080/v1/audio/transcriptions");
        assert_eq!(req.timeout_ms, NETWORK_MAX_TIMEOUT_MS);
        assert!(String::from_utf8_lossy(&decoded_body(&req)).contains("filename=\"audio.ogg\""));
    }

    #[test]
    fn multipart_audio_bytes_roundtrip_exactly() {
        let audio = vec![0u8, 1, 2, 0xff, 0xfe, 0x7f, 0x80, 0x40];
        let mut p = params(AudioFormat::Wav);
        p.audio = audio.clone();
        let req = OpenAiProvider.build_http_request(&p, "sk-test");
        let body = decoded_body(&req);
        // The raw audio bytes must appear verbatim (not base64 or mangled)
        // between the file headers and the closing boundary.
        assert!(
            body.windows(audio.len()).any(|w| w == audio),
            "audio bytes must appear verbatim in the multipart body"
        );
    }

    #[test]
    fn parse_response_json_text() {
        let p = params(AudioFormat::Wav);
        let result = OpenAiProvider
            .parse_response(br#"{"text": "  Hello world.  "}"#, &p)
            .unwrap();
        assert_eq!(result.text, "Hello world.");
        assert_eq!(result.model, "whisper-1");
        assert_eq!(result.language, "");
    }

    #[test]
    fn parse_response_echoes_language_hint_and_model_override() {
        let mut p = params(AudioFormat::Mp3);
        p.language = Some("de".to_string());
        p.model = Some("gpt-4o-transcribe".to_string());
        let result = OpenAiProvider
            .parse_response(br#"{"text": "Hallo"}"#, &p)
            .unwrap();
        assert_eq!(result.language, "de");
        assert_eq!(result.model, "gpt-4o-transcribe");
        assert_eq!(result.duration_seconds, 0.0, "mp3 duration not derivable");
    }

    #[test]
    fn parse_response_wav_duration_from_input() {
        // 16000 Hz mono 16-bit wav, 64000 data bytes -> 2.0 s
        let wav = crate::provider::sherpa::tests::fixture_wav(16000, 1, 64000);
        let mut p = params(AudioFormat::Wav);
        p.audio = wav;
        let result = OpenAiProvider
            .parse_response(br#"{"text": "hi"}"#, &p)
            .unwrap();
        assert_eq!(result.duration_seconds, 2.0);
    }

    #[test]
    fn parse_response_rejects_empty_text() {
        let p = params(AudioFormat::Wav);
        let err = OpenAiProvider
            .parse_response(br#"{"text": "   "}"#, &p)
            .unwrap_err();
        assert!(err.contains("empty transcript"), "error was: {err}");
    }

    #[test]
    fn parse_response_rejects_malformed_json() {
        let p = params(AudioFormat::Wav);
        let err = OpenAiProvider.parse_response(b"not json", &p).unwrap_err();
        assert!(err.contains("malformed"), "error was: {err}");
    }

    #[test]
    fn fresh_boundaries_differ() {
        assert_ne!(fresh_boundary(), fresh_boundary());
    }
}
