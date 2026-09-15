//! Parse + validate the JSON body of an `stt_transcribe` / `stt_models`
//! `ActionRequest`.

/// Action-level timeout ceiling. Local transcription (sherpa) is CPU-bound
/// and can exceed the 30 s HTTP cap, so the action timeout is higher than
/// `network`'s own cap; cloud requests are additionally bounded by
/// [`NETWORK_MAX_TIMEOUT_MS`] when they hit `network`.
pub const MAX_TIMEOUT_MS: u64 = 60_000;

/// Default `timeout_ms` when the caller omits it.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Hard ceiling `network` applies to its own `http_request` action — a
/// cloud transcription is clamped to this when routed through it.
pub const NETWORK_MAX_TIMEOUT_MS: u64 = 30_000;

/// Cap on the base64-encoded audio in one request. Keeps the decoded audio
/// under `network`'s 25 MiB body limit even after base64 inflation (~33%)
/// and multipart framing, so a transcribe upload always fits.
pub const MAX_AUDIO_B64_LEN: usize = 25 * 1024 * 1024;

/// Cap on the optional Whisper-style context hint.
pub const MAX_PROMPT_CHARS: usize = 1000;

/// Temperature clamps; passed through to cloud providers.
pub const MIN_TEMPERATURE: f32 = 0.0;
pub const MAX_TEMPERATURE: f32 = 1.0;

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENAI_MODEL: &str = "whisper-1";

/// OpenAI transcription models known at the time of writing. Kept as a
/// strict list so typos fail fast; the error message names the list. When
/// OpenAI ships a new model, callers must bump this (or list it via
/// `stt_models`, which mirrors this list).
pub const OPENAI_MODELS: [&str; 3] = ["whisper-1", "gpt-4o-transcribe", "gpt-4o-mini-transcribe"];

/// Operator-supplied allowlist of env var names a caller's `api_key_env`
/// may name. Comma-separated, exact (case-sensitive) match. Default-deny:
/// unset or empty means no `api_key_env` value is accepted for cloud
/// providers — a caller could otherwise name *any* environment variable in
/// the `stt` process (an unrelated secret, not just a provider key) and
/// have its value sent straight into an outbound request header to a
/// caller-controlled `base_url`, exfiltrating it.
pub const ALLOWED_KEY_ENVS_ENV: &str = "STT_PLUGIN_ALLOWED_KEY_ENVS";

/// Parse [`ALLOWED_KEY_ENVS_ENV`]'s raw value into the set of permitted
/// `api_key_env` names.
pub fn parse_allowed_key_envs(raw: &str) -> std::collections::HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// True if `name` is permitted as an `api_key_env` value, per the
/// operator's [`ALLOWED_KEY_ENVS_ENV`] allowlist.
pub fn is_allowed_key_env(name: &str, allowed: &std::collections::HashSet<String>) -> bool {
    allowed.contains(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Sherpa,
    OpenAi,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Sherpa => "sherpa",
            Provider::OpenAi => "openai",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    Wav,
    Pcm,
    Ogg,
}

impl AudioFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Wav => "wav",
            AudioFormat::Pcm => "pcm",
            AudioFormat::Ogg => "ogg",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TranscribeParams {
    pub provider: Provider,
    /// The decoded audio bytes (base64 in the JSON, decoded here).
    pub audio: Vec<u8>,
    pub format: AudioFormat,
    /// For raw `pcm` input: the sample rate and channel count (ignored for
    /// container formats like wav, which carry their own).
    pub sample_rate_hz: u32,
    pub num_channels: u16,
    /// ISO-639-1 hint (caller-declared). Echoed back; for `openai` also
    /// sent to the provider.
    pub language: Option<String>,
    /// Whisper-style context hint, sent to the cloud provider only.
    pub prompt: Option<String>,
    pub temperature: Option<f32>,
    /// Name of an env var the `stt` process reads at call time (cloud
    /// providers only; ignored for `sherpa`). Never a literal key.
    pub api_key_env: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub timeout_ms: u64,
}

/// Parse and validate `params_json` for the `stt_transcribe` action.
pub fn parse_request(params_json: &[u8]) -> Result<TranscribeParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        provider: Option<String>,
        audio_base64: Option<String>,
        file_path: Option<String>,
        format: Option<String>,
        sample_rate_hz: Option<u32>,
        num_channels: Option<u16>,
        language: Option<String>,
        prompt: Option<String>,
        temperature: Option<f32>,
        api_key_env: Option<String>,
        base_url: Option<String>,
        model: Option<String>,
        timeout_ms: Option<u64>,
    }

    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let provider = match raw.provider.as_deref() {
        Some("sherpa") => Provider::Sherpa,
        Some("openai") => Provider::OpenAi,
        Some(other) => return Err(format!("unsupported provider: {other}")),
        None => return Err("missing required field: provider".to_string()),
    };

    let inferred_format = raw.format.clone().or_else(|| {
        raw.file_path.as_ref().and_then(|p| {
            let ext = p.rsplit('.').next()?.to_lowercase();
            match ext.as_str() {
                "ogg" => Some("ogg".to_string()),
                "mp3" => Some("mp3".to_string()),
                "wav" => Some("wav".to_string()),
                "pcm" => Some("pcm".to_string()),
                _ => None,
            }
        })
    });
    let has_file_path = raw.file_path.is_some();
    let audio = match (raw.audio_base64, raw.file_path) {
        (None, None) => return Err("missing required field: audio_base64 or file_path".to_string()),
        (Some(b64), _) => {
            if b64.is_empty() {
                return Err("audio_base64 must not be empty".to_string());
            }
            if b64.len() > MAX_AUDIO_B64_LEN {
                return Err(format!(
                    "audio_base64 exceeds max size of {} bytes",
                    MAX_AUDIO_B64_LEN
                ));
            }
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .map_err(|e| format!("audio_base64 is not valid base64: {e}"))?;
            if bytes.is_empty() {
                return Err("audio_base64 decoded to an empty audio payload".to_string());
            }
            bytes
        }
        (None, Some(path)) => {
            let p = path.trim();
            if p.is_empty() {
                return Err("file_path must not be empty".to_string());
            }
            let bytes = std::fs::read(p).map_err(|e| format!("failed to read file_path '{}': {e}", p))?;
            if bytes.is_empty() {
                return Err(format!("file_path '{}' is empty", p));
            }
            if bytes.len() > 25 * 1024 * 1024 {
                return Err(format!("file_path '{}' exceeds 25 MiB", p));
            }
            bytes
        }
    };
    let format = match (provider, inferred_format.as_deref()) {
        (Provider::Sherpa, None | Some("wav")) => AudioFormat::Wav,
        (Provider::Sherpa, Some("pcm")) => AudioFormat::Pcm,
        (Provider::Sherpa, Some("ogg") | Some("mp3")) if has_file_path => AudioFormat::Ogg,
        (Provider::Sherpa, Some(other)) => {
            return Err(format!("sherpa supports formats wav|pcm, got: {other}"))
        }
        (Provider::OpenAi, None | Some("wav")) => AudioFormat::Wav,
        (Provider::OpenAi, Some("mp3")) => AudioFormat::Mp3,
        (Provider::OpenAi, Some("ogg")) => AudioFormat::Ogg,
        (Provider::OpenAi, Some("pcm")) => {
            return Err("openai supports formats wav|mp3|ogg (raw pcm is not accepted)".to_string())
        }
        (Provider::OpenAi, Some(other)) => {
            return Err(format!("openai supports formats wav|mp3|ogg, got: {other}"))
        }
    };

    let language = match raw.language {
        None => None,
        Some(lang) => {
            let lang = lang.trim().to_lowercase();
            if lang.is_empty() {
                None
            } else if lang.len() > 10 || !lang.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err(format!(
                    "language '{lang}' is not a valid ISO-639-1 code (letters only)"
                ));
            } else {
                Some(lang)
            }
        }
    };

    let prompt = match raw.prompt {
        None => None,
        Some(p) => {
            let p = p.trim().to_string();
            if p.is_empty() {
                None
            } else if p.chars().count() > MAX_PROMPT_CHARS {
                return Err(format!(
                    "prompt exceeds max length of {MAX_PROMPT_CHARS} chars"
                ));
            } else {
                Some(p)
            }
        }
    };

    let temperature = raw
        .temperature
        .map(|t| t.clamp(MIN_TEMPERATURE, MAX_TEMPERATURE));

    // Cloud providers need an allowlisted env var name; local does not.
    let api_key_env = match (provider, raw.api_key_env) {
        (Provider::Sherpa, _) => String::new(),
        (_, None) => return Err("missing required field: api_key_env".to_string()),
        (_, Some(k)) if k.is_empty() => return Err("api_key_env must not be empty".to_string()),
        (_, Some(k)) => k,
    };

    let base_url = raw.base_url.filter(|u| !u.is_empty());
    let model = raw.model.filter(|m| !m.is_empty());
    if let (Provider::OpenAi, Some(m)) = (provider, model.as_deref()) {
        if !OPENAI_MODELS.contains(&m) {
            return Err(format!(
                "unknown openai model '{m}' (known: {})",
                OPENAI_MODELS.join(", ")
            ));
        }
    }

    let timeout_ms = raw
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);

    Ok(TranscribeParams {
        provider,
        audio,
        format,
        sample_rate_hz: raw.sample_rate_hz.unwrap_or(0),
        num_channels: raw.num_channels.unwrap_or(0),
        language,
        prompt,
        temperature,
        api_key_env,
        base_url,
        model,
        timeout_ms,
    })
}

/// Parse + validate the `stt_models` request: just a provider, and it must
/// be one whose model list is knowable without a live provider call.
pub fn parse_models_request(params_json: &[u8]) -> Result<Provider, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        provider: Option<String>,
    }
    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;
    match raw.provider.as_deref() {
        Some("sherpa") => Ok(Provider::Sherpa),
        Some("openai") => Ok(Provider::OpenAi),
        Some(other) => Err(format!("unsupported provider: {other}")),
        None => Err("missing required field: provider".to_string()),
    }
}

/// Default `stt_listen_*` stream id when the caller omits it.
pub const LISTEN_DEFAULT_STREAM_ID: u32 = 1;

/// Parameters for the `stt_listen_start` action: open an accumulation
/// buffer for an inbound PCM audio stream.
#[derive(Debug, Clone)]
pub struct ListenStartParams {
    pub stream_id: u32,
    pub sample_rate_hz: u32,
    pub num_channels: u16,
    /// ISO-639-1 hint applied at transcription time.
    pub language: Option<String>,
}

/// Parse + validate the `stt_listen_start` request.
pub fn parse_listen_start_request(params_json: &[u8]) -> Result<ListenStartParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        stream_id: Option<u32>,
        sample_rate_hz: Option<u32>,
        num_channels: Option<u16>,
        language: Option<String>,
    }
    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let sample_rate_hz = raw
        .sample_rate_hz
        .ok_or("missing required field: sample_rate_hz")?;
    if sample_rate_hz == 0 {
        return Err("sample_rate_hz must be > 0".to_string());
    }
    let num_channels = raw.num_channels.unwrap_or(1);
    if num_channels == 0 {
        return Err("num_channels must be > 0".to_string());
    }

    Ok(ListenStartParams {
        stream_id: raw.stream_id.unwrap_or(LISTEN_DEFAULT_STREAM_ID),
        sample_rate_hz,
        num_channels,
        language: raw.language.filter(|l| !l.is_empty()),
    })
}

/// Parameters for the `stt_listen_stop` action: transcribe the accumulated
/// buffer for one stream.
#[derive(Debug, Clone)]
pub struct ListenStopParams {
    pub stream_id: u32,
}

/// Parse + validate the `stt_listen_stop` request.
pub fn parse_listen_stop_request(params_json: &[u8]) -> Result<ListenStopParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        stream_id: Option<u32>,
    }
    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;
    Ok(ListenStopParams {
        stream_id: raw.stream_id.unwrap_or(LISTEN_DEFAULT_STREAM_ID),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn valid_sherpa_json() -> serde_json::Value {
        serde_json::json!({
            "provider": "sherpa",
            "audio_base64": b64(b"RIFF"),
        })
    }

    fn valid_openai_json() -> serde_json::Value {
        serde_json::json!({
            "provider": "openai",
            "audio_base64": b64(b"RIFF"),
            "api_key_env": "OPENAI_API_KEY",
        })
    }

    #[test]
    fn accepts_minimal_sherpa_request() {
        let params = parse_request(valid_sherpa_json().to_string().as_bytes()).unwrap();
        assert_eq!(params.provider, Provider::Sherpa);
        assert_eq!(params.format, AudioFormat::Wav);
        assert_eq!(params.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(params.api_key_env.is_empty());
        assert_eq!(params.audio, b"RIFF");
    }

    #[test]
    fn sherpa_accepts_pcm_format_with_metadata() {
        let mut body = valid_sherpa_json();
        body["format"] = "pcm".into();
        body["sample_rate_hz"] = 16000.into();
        body["num_channels"] = 1.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.format, AudioFormat::Pcm);
        assert_eq!(params.sample_rate_hz, 16000);
        assert_eq!(params.num_channels, 1);
    }

    #[test]
    fn sherpa_rejects_mp3_and_ogg() {
        let mut body = valid_sherpa_json();
        body["format"] = "mp3".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("sherpa supports formats"), "error was: {err}");

        let mut body = valid_sherpa_json();
        body["format"] = "ogg".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("sherpa supports formats"), "error was: {err}");
    }

    #[test]
    fn accepts_minimal_openai_request() {
        let params = parse_request(valid_openai_json().to_string().as_bytes()).unwrap();
        assert_eq!(params.provider, Provider::OpenAi);
        assert_eq!(params.format, AudioFormat::Wav);
        assert_eq!(params.base_url, None);
        assert_eq!(params.model, None);
    }

    #[test]
    fn openai_accepts_mp3_and_ogg() {
        let mut body = valid_openai_json();
        body["format"] = "mp3".into();
        assert_eq!(
            parse_request(body.to_string().as_bytes()).unwrap().format,
            AudioFormat::Mp3
        );
        let mut body = valid_openai_json();
        body["format"] = "ogg".into();
        assert_eq!(
            parse_request(body.to_string().as_bytes()).unwrap().format,
            AudioFormat::Ogg
        );
    }

    #[test]
    fn openai_rejects_raw_pcm() {
        let mut body = valid_openai_json();
        body["format"] = "pcm".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("raw pcm"), "error was: {err}");
    }

    #[test]
    fn openai_rejects_unknown_model() {
        let mut body = valid_openai_json();
        body["model"] = "whisper-9".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("unknown openai model"), "error was: {err}");
    }

    #[test]
    fn openai_accepts_known_model_override() {
        let mut body = valid_openai_json();
        body["model"] = "gpt-4o-transcribe".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.model.as_deref(), Some("gpt-4o-transcribe"));
    }

    #[test]
    fn openai_requires_api_key_env() {
        let mut body = valid_openai_json();
        body.as_object_mut().unwrap().remove("api_key_env");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("api_key_env"), "error was: {err}");
    }

    #[test]
    fn rejects_missing_provider() {
        let mut body = valid_sherpa_json();
        body.as_object_mut().unwrap().remove("provider");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("provider"), "error was: {err}");
    }

    #[test]
    fn rejects_unsupported_provider() {
        let mut body = valid_sherpa_json();
        body["provider"] = "google".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("unsupported provider"), "error was: {err}");
    }

    #[test]
    fn rejects_missing_audio() {
        let mut body = valid_sherpa_json();
        body.as_object_mut().unwrap().remove("audio_base64");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("audio_base64"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_audio() {
        let mut body = valid_sherpa_json();
        body["audio_base64"] = "".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("audio_base64"), "error was: {err}");
    }

    #[test]
    fn rejects_invalid_base64() {
        let mut body = valid_sherpa_json();
        body["audio_base64"] = "!!not base64!!".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("valid base64"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_audio() {
        let mut body = valid_sherpa_json();
        body["audio_base64"] = "A".repeat(MAX_AUDIO_B64_LEN + 1).into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("max size"), "error was: {err}");
    }

    #[test]
    fn normalizes_language_to_lowercase() {
        let mut body = valid_sherpa_json();
        body["language"] = "DE".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.language.as_deref(), Some("de"));
    }

    #[test]
    fn rejects_invalid_language() {
        let mut body = valid_sherpa_json();
        body["language"] = "de-DE".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("ISO-639-1"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_prompt() {
        let mut body = valid_sherpa_json();
        body["prompt"] = "x".repeat(MAX_PROMPT_CHARS + 1).into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("max length"), "error was: {err}");
    }

    #[test]
    fn clamps_temperature() {
        let mut body = valid_openai_json();
        body["temperature"] = 99.0.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.temperature, Some(MAX_TEMPERATURE));
        body["temperature"] = (-1.0).into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.temperature, Some(MIN_TEMPERATURE));
    }

    #[test]
    fn clamps_timeout_ms_above_cap() {
        let mut body = valid_sherpa_json();
        body["timeout_ms"] = 999_999.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn allowed_key_envs_empty_by_default() {
        assert!(parse_allowed_key_envs("").is_empty());
    }

    #[test]
    fn allowed_key_envs_parses_comma_list() {
        let allowed = parse_allowed_key_envs("OPENAI_API_KEY, ANOTHER_KEY ,,");
        assert!(is_allowed_key_env("OPENAI_API_KEY", &allowed));
        assert!(is_allowed_key_env("ANOTHER_KEY", &allowed));
        assert_eq!(allowed.len(), 2);
    }

    #[test]
    fn is_allowed_key_env_rejects_unlisted_name() {
        let allowed = parse_allowed_key_envs("OPENAI_API_KEY");
        assert!(!is_allowed_key_env("AWS_SECRET_ACCESS_KEY", &allowed));
    }

    #[test]
    fn is_allowed_key_env_is_case_sensitive() {
        let allowed = parse_allowed_key_envs("OPENAI_API_KEY");
        assert!(!is_allowed_key_env("openai_api_key", &allowed));
    }

    #[test]
    fn models_request_accepts_sherpa_and_openai() {
        assert_eq!(
            parse_models_request(br#"{"provider":"sherpa"}"#).unwrap(),
            Provider::Sherpa
        );
        assert_eq!(
            parse_models_request(br#"{"provider":"openai"}"#).unwrap(),
            Provider::OpenAi
        );
    }

    #[test]
    fn models_request_rejects_unknown_provider() {
        let err = parse_models_request(br#"{"provider":"google"}"#).unwrap_err();
        assert!(err.contains("unsupported provider"), "error was: {err}");
    }

    #[test]
    fn listen_start_accepts_minimal_request() {
        let params = parse_listen_start_request(br#"{"sample_rate_hz":16000}"#).unwrap();
        assert_eq!(params.stream_id, LISTEN_DEFAULT_STREAM_ID);
        assert_eq!(params.sample_rate_hz, 16_000);
        assert_eq!(params.num_channels, 1);
        assert_eq!(params.language, None);
    }

    #[test]
    fn listen_start_accepts_overrides() {
        let params = parse_listen_start_request(
            br#"{"stream_id":3,"sample_rate_hz":24000,"num_channels":2,"language":"en"}"#,
        )
        .unwrap();
        assert_eq!(params.stream_id, 3);
        assert_eq!(params.sample_rate_hz, 24_000);
        assert_eq!(params.num_channels, 2);
        assert_eq!(params.language.as_deref(), Some("en"));
    }

    #[test]
    fn listen_start_requires_sample_rate() {
        let err = parse_listen_start_request(br#"{}"#).unwrap_err();
        assert!(err.contains("sample_rate_hz"), "error was: {err}");
        let err = parse_listen_start_request(br#"{"sample_rate_hz":0}"#).unwrap_err();
        assert!(err.contains("> 0"), "error was: {err}");
    }

    #[test]
    fn listen_start_rejects_zero_channels() {
        let err = parse_listen_start_request(br#"{"sample_rate_hz":16000,"num_channels":0}"#)
            .unwrap_err();
        assert!(err.contains("> 0"), "error was: {err}");
    }

    #[test]
    fn listen_stop_defaults_stream_id() {
        let params = parse_listen_stop_request(br#"{}"#).unwrap();
        assert_eq!(params.stream_id, LISTEN_DEFAULT_STREAM_ID);
        let params = parse_listen_stop_request(br#"{"stream_id":9}"#).unwrap();
        assert_eq!(params.stream_id, 9);
    }
}
