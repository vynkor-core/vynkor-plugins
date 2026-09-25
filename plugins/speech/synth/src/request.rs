//! Parse + validate the JSON body of a `tts_synthesize` / `tts_voices`
//! `ActionRequest`.

/// Action-level timeout ceiling. Local synthesis (sherpa) is CPU-bound and
/// can exceed the 30 s HTTP cap, so the action timeout is higher than
/// `network`'s own cap; cloud requests are additionally bounded by
/// [`NETWORK_MAX_TIMEOUT_MS`] when they hit `network`.
pub const MAX_TIMEOUT_MS: u64 = 60_000;

/// Default `timeout_ms` when the caller omits it.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Hard ceiling `network` applies to its own `http_request` action — a
/// cloud synthesize call is clamped to this when routed through it.
pub const NETWORK_MAX_TIMEOUT_MS: u64 = 30_000;

/// Hard ceiling on input text length, in chars. Bounds the memory a single
/// local synthesis can consume (~10 MB of samples per 100 s of audio at
/// 24 kHz) and matches cloud provider input limits.
pub const MAX_TEXT_CHARS: usize = 4000;

/// Speed clamps; passed through to providers.
pub const MIN_SPEED: f32 = 0.25;
pub const MAX_SPEED: f32 = 4.0;
pub const DEFAULT_SPEED: f32 = 1.0;

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini-tts";
pub const DEFAULT_ELEVENLABS_BASE_URL: &str = "https://api.elevenlabs.io";
pub const DEFAULT_ELEVENLABS_MODEL: &str = "eleven_multilingual_v2";

/// Operator-supplied allowlist of env var names a caller's `api_key_env`
/// may name. Comma-separated, exact (case-sensitive) match. Default-deny:
/// unset or empty means no `api_key_env` value is accepted for cloud
/// providers — a caller could otherwise name *any* environment variable in
/// the `tts` process (an unrelated secret, not just a provider key) and
/// have its value sent straight into an outbound request header to a
/// caller-controlled `base_url`, exfiltrating it.
pub const ALLOWED_KEY_ENVS_ENV: &str = "TTS_PLUGIN_ALLOWED_KEY_ENVS";

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

/// OpenAI voice ids known at the time of writing. Kept as a strict list so
/// typos fail fast; the error message names the list. When OpenAI ships a
/// new voice, callers must bump this (or the model they target won't
/// accept it anyway).
pub const OPENAI_VOICES: [&str; 12] = [
    "alloy", "ash", "ballad", "coral", "echo", "fable", "onyx", "nova", "sage", "shimmer", "verse",
    "amethyst",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Sherpa,
    OpenAi,
    ElevenLabs,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Sherpa => "sherpa",
            Provider::OpenAi => "openai",
            Provider::ElevenLabs => "elevenlabs",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    Wav,
    Pcm,
    Opus,
    Aac,
    Flac,
    Ulaw,
}

impl AudioFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Wav => "wav",
            AudioFormat::Pcm => "pcm",
            AudioFormat::Opus => "opus",
            AudioFormat::Aac => "aac",
            AudioFormat::Flac => "flac",
            AudioFormat::Ulaw => "ulaw",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SynthesizeParams {
    pub provider: Provider,
    pub text: String,
    pub voice: String,
    /// Name of an env var the `tts` process reads at call time (cloud
    /// providers only; ignored for `sherpa`). Never a literal key.
    pub api_key_env: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub format: AudioFormat,
    pub speed: f32,
    pub timeout_ms: u64,
}

/// Parse and validate `params_json` for the `tts_synthesize` action.
pub fn parse_request(params_json: &[u8]) -> Result<SynthesizeParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        provider: Option<String>,
        text: Option<String>,
        voice: Option<String>,
        api_key_env: Option<String>,
        base_url: Option<String>,
        model: Option<String>,
        format: Option<String>,
        speed: Option<f32>,
        timeout_ms: Option<u64>,
    }

    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let provider = match raw.provider.as_deref() {
        Some("sherpa") => Provider::Sherpa,
        Some("openai") => Provider::OpenAi,
        Some("elevenlabs") => Provider::ElevenLabs,
        Some(other) => return Err(format!("unsupported provider: {other}")),
        None => return Err("missing required field: provider".to_string()),
    };

    let text = raw.text.ok_or("missing required field: text")?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("text must not be empty".to_string());
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!(
            "text exceeds max length of {MAX_TEXT_CHARS} chars (got {})",
            text.chars().count()
        ));
    }

    let voice = raw.voice.ok_or("missing required field: voice")?;
    if voice.is_empty() {
        return Err("voice must not be empty".to_string());
    }
    if provider == Provider::OpenAi && !OPENAI_VOICES.contains(&voice.as_str()) {
        return Err(format!(
            "unknown openai voice '{voice}' (known: {})",
            OPENAI_VOICES.join(", ")
        ));
    }

    // Cloud providers need an allowlisted env var name; local does not.
    let api_key_env = match (provider, raw.api_key_env) {
        (Provider::Sherpa, _) => String::new(),
        (_, None) => return Err("missing required field: api_key_env".to_string()),
        (_, Some(k)) if k.is_empty() => return Err("api_key_env must not be empty".to_string()),
        (_, Some(k)) => k,
    };

    let base_url = raw.base_url.filter(|u| !u.is_empty());
    let model = raw.model.filter(|m| !m.is_empty());

    let format = match (provider, raw.format.as_deref()) {
        (Provider::Sherpa, None | Some("wav")) => AudioFormat::Wav,
        (Provider::Sherpa, Some("pcm")) => AudioFormat::Pcm,
        (Provider::Sherpa, Some("mp3")) => AudioFormat::Mp3,
        (Provider::Sherpa, Some(other)) => {
            return Err(format!("sherpa supports formats wav|pcm|mp3, got: {other}"))
        }
        (Provider::OpenAi, None | Some("mp3")) => AudioFormat::Mp3,
        (Provider::OpenAi, Some("wav")) => AudioFormat::Wav,
        (Provider::OpenAi, Some("pcm")) => AudioFormat::Pcm,
        (Provider::OpenAi, Some("opus")) => AudioFormat::Opus,
        (Provider::OpenAi, Some("aac")) => AudioFormat::Aac,
        (Provider::OpenAi, Some("flac")) => AudioFormat::Flac,
        (Provider::OpenAi, Some(other)) => {
            return Err(format!("openai supports formats mp3|wav|pcm|opus|aac|flac, got: {other}"))
        }
        (Provider::ElevenLabs, None | Some("mp3")) => AudioFormat::Mp3,
        (Provider::ElevenLabs, Some("pcm")) => AudioFormat::Pcm,
        (Provider::ElevenLabs, Some("ulaw")) => AudioFormat::Ulaw,
        (Provider::ElevenLabs, Some(other)) => {
            return Err(format!("elevenlabs supports formats mp3|pcm|ulaw, got: {other}"))
        }
    };

    let speed = raw
        .speed
        .unwrap_or(DEFAULT_SPEED)
        .clamp(MIN_SPEED, MAX_SPEED);
    let timeout_ms = raw
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);

    Ok(SynthesizeParams {
        provider,
        text,
        voice,
        api_key_env,
        base_url,
        model,
        format,
        speed,
        timeout_ms,
    })
}

/// Parse + validate the `tts_voices` request: just a provider, and it must
/// be one whose voice list is knowable without a live provider call.
pub fn parse_voices_request(params_json: &[u8]) -> Result<Provider, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        provider: Option<String>,
    }
    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;
    match raw.provider.as_deref() {
        Some("sherpa") => Ok(Provider::Sherpa),
        Some("openai") => Ok(Provider::OpenAi),
        Some("elevenlabs") => Err(
            "elevenlabs voices are per-account; list them via the ElevenLabs dashboard or \
             GET /v1/voices with an account key"
                .to_string(),
        ),
        Some(other) => Err(format!("unsupported provider: {other}")),
        None => Err("missing required field: provider".to_string()),
    }
}

/// Opus stream rates the speak path accepts (sherpa synthesizes at the
/// model's rate; 24 kHz is the common Kokoro output and Opus's native
/// speech sweet spot).
pub const SPEAK_DEFAULT_SAMPLE_RATE: u32 = 24_000;

/// Default `tts_speak` stream id when the caller omits it.
pub const SPEAK_DEFAULT_STREAM_ID: u32 = 1;

/// Default Opus bitrate for `tts_speak`; see `crate::provider::opus`.
pub const SPEAK_DEFAULT_BITRATE: i32 = 32_000;

/// Lower bound on the caller-supplied target length. The target is a
/// kernel-routed plugin id / capability (`phone-1.speaker`, ...); a
/// bare minimum keeps typos from silently addressing the kernel.
pub const MIN_TARGET_CHARS: usize = 1;

/// Parameters for the `tts_speak` action: synthesize then stream the audio
/// as Opus `AudioStreamChunk`s to a peer plugin (the client speaker).
#[derive(Debug, Clone)]
pub struct SpeakParams {
    pub text: String,
    pub voice: String,
    /// Peer to stream the Opus packets to (e.g. `phone-1.speaker`).
    pub target: String,
    /// Caller-chosen stream id echoed in every `AudioStreamChunk`.
    pub stream_id: u32,
    /// PCM sample rate the encoded stream is advertised at. `0` = use the
    /// model's natural output rate.
    pub sample_rate_hz: u32,
    /// Opus encode bitrate; `0` = codec default.
    pub bitrate: i32,
    pub speed: f32,
    pub timeout_ms: u64,
}

/// Parse + validate the `tts_speak` request. Local-only for now: streaming
/// needs a PCM source, which only the sherpa provider has (cloud adapters
/// return opaque mp3/ogg containers).
pub fn parse_speak_request(params_json: &[u8]) -> Result<SpeakParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        provider: Option<String>,
        text: Option<String>,
        voice: Option<String>,
        target: Option<String>,
        stream_id: Option<u32>,
        sample_rate_hz: Option<u32>,
        bitrate: Option<i32>,
        speed: Option<f32>,
        timeout_ms: Option<u64>,
    }

    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;

    match raw.provider.as_deref() {
        Some("sherpa") => {}
        Some(other) => return Err(format!("tts_speak is sherpa-only for now, got: {other}")),
        None => return Err("missing required field: provider".to_string()),
    }

    let text = raw.text.ok_or("missing required field: text")?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("text must not be empty".to_string());
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!(
            "text exceeds max length of {MAX_TEXT_CHARS} chars (got {})",
            text.chars().count()
        ));
    }

    let voice = raw.voice.ok_or("missing required field: voice")?;
    if voice.is_empty() {
        return Err("voice must not be empty".to_string());
    }

    let target = raw.target.ok_or("missing required field: target")?;
    if target.chars().count() < MIN_TARGET_CHARS {
        return Err("target must not be empty".to_string());
    }

    let speed = raw
        .speed
        .unwrap_or(DEFAULT_SPEED)
        .clamp(MIN_SPEED, MAX_SPEED);
    let timeout_ms = raw
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);

    Ok(SpeakParams {
        text,
        voice,
        target,
        stream_id: raw.stream_id.unwrap_or(SPEAK_DEFAULT_STREAM_ID),
        sample_rate_hz: raw.sample_rate_hz.unwrap_or(SPEAK_DEFAULT_SAMPLE_RATE),
        bitrate: raw.bitrate.unwrap_or(SPEAK_DEFAULT_BITRATE),
        speed,
        timeout_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_sherpa_json() -> serde_json::Value {
        serde_json::json!({
            "provider": "sherpa",
            "text": "Hello, world!",
            "voice": "af_heart",
        })
    }

    fn valid_openai_json() -> serde_json::Value {
        serde_json::json!({
            "provider": "openai",
            "text": "Hello, world!",
            "voice": "alloy",
            "api_key_env": "OPENAI_API_KEY",
        })
    }

    #[test]
    fn accepts_minimal_sherpa_request() {
        let params = parse_request(valid_sherpa_json().to_string().as_bytes()).unwrap();
        assert_eq!(params.provider, Provider::Sherpa);
        assert_eq!(params.format, AudioFormat::Wav);
        assert_eq!(params.speed, DEFAULT_SPEED);
        assert_eq!(params.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(params.api_key_env.is_empty());
    }

    #[test]
    fn sherpa_accepts_pcm_format() {
        let mut body = valid_sherpa_json();
        body["format"] = "pcm".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.format, AudioFormat::Pcm);
    }

    #[test]
    fn sherpa_accepts_mp3_format() {
        let mut body = valid_sherpa_json();
        body["format"] = "mp3".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.format, AudioFormat::Mp3);
    }

    #[test]
    fn sherpa_rejects_opus_format() {
        let mut body = valid_sherpa_json();
        body["format"] = "opus".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("sherpa supports formats"), "error was: {err}");
    }

    #[test]
    fn accepts_minimal_openai_request() {
        let params = parse_request(valid_openai_json().to_string().as_bytes()).unwrap();
        assert_eq!(params.provider, Provider::OpenAi);
        assert_eq!(params.format, AudioFormat::Mp3);
        assert_eq!(params.base_url, None);
        assert_eq!(params.model, None);
    }

    #[test]
    fn openai_rejects_unknown_voice() {
        let mut body = valid_openai_json();
        body["voice"] = "alloi".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("unknown openai voice"), "error was: {err}");
    }

    #[test]
    fn openai_requires_api_key_env() {
        let mut body = valid_openai_json();
        body.as_object_mut().unwrap().remove("api_key_env");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("api_key_env"), "error was: {err}");
    }

    #[test]
    fn elevenlabs_accepts_any_voice_id() {
        let body = serde_json::json!({
            "provider": "elevenlabs",
            "text": "hi",
            "voice": "21m00Tcm4TlvDq8ikWAM",
            "api_key_env": "ELEVENLABS_API_KEY",
            "format": "pcm",
        });
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.provider, Provider::ElevenLabs);
        assert_eq!(params.format, AudioFormat::Pcm);
    }

    #[test]
    fn elevenlabs_rejects_wav_format() {
        let mut body = serde_json::json!({
            "provider": "elevenlabs",
            "text": "hi",
            "voice": "x",
            "api_key_env": "ELEVENLABS_API_KEY",
        });
        body["format"] = "wav".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(
            err.contains("elevenlabs supports formats"),
            "error was: {err}"
        );
    }

    #[test]
    fn openai_accepts_opus_aac_flac() {
        for f in ["opus", "aac", "flac"] {
            let mut body = valid_openai_json();
            body["format"] = f.into();
            let params = parse_request(body.to_string().as_bytes()).unwrap();
            assert_eq!(params.format.as_str(), f);
        }
    }

    #[test]
    fn elevenlabs_accepts_ulaw() {
        let mut body = serde_json::json!({
            "provider": "elevenlabs",
            "text": "hi",
            "voice": "x",
            "api_key_env": "ELEVENLABS_API_KEY",
        });
        body["format"] = "ulaw".into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.format, AudioFormat::Ulaw);
    }

    #[test]
    fn new_formats_rejected_by_wrong_provider() {
        let cases: [(&str, &str); 6] = [
            ("sherpa", "opus"),
            ("sherpa", "aac"),
            ("sherpa", "flac"),
            ("sherpa", "ulaw"),
            ("openai", "ulaw"),
            ("elevenlabs", "opus"),
        ];
        for (provider, format) in cases {
            let mut body = match provider {
                "sherpa" => valid_sherpa_json(),
                "openai" => valid_openai_json(),
                _ => serde_json::json!({
                    "provider": "elevenlabs",
                    "text": "hi",
                    "voice": "x",
                    "api_key_env": "ELEVENLABS_API_KEY",
                }),
            };
            body["format"] = format.into();
            let err = parse_request(body.to_string().as_bytes()).unwrap_err();
            assert!(
                err.contains(&format!("{provider} supports formats")),
                "provider {provider} + format {format}: error was {err}"
            );
        }
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
    fn rejects_missing_text() {
        let mut body = valid_sherpa_json();
        body.as_object_mut().unwrap().remove("text");
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("text"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_text() {
        let mut body = valid_sherpa_json();
        body["text"] = "   ".into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("text"), "error was: {err}");
    }

    #[test]
    fn rejects_oversized_text() {
        let mut body = valid_sherpa_json();
        body["text"] = "x".repeat(MAX_TEXT_CHARS + 1).into();
        let err = parse_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("max length"), "error was: {err}");
    }

    #[test]
    fn clamps_speed() {
        let mut body = valid_sherpa_json();
        body["speed"] = 99.0.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.speed, MAX_SPEED);
        body["speed"] = 0.0.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.speed, MIN_SPEED);
    }

    #[test]
    fn clamps_timeout_ms_above_cap() {
        let mut body = valid_sherpa_json();
        body["timeout_ms"] = 999_999.into();
        let params = parse_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(params.timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn openai_defaults_apply_when_omitted() {
        let params = parse_request(valid_openai_json().to_string().as_bytes()).unwrap();
        assert_eq!(params.speed, DEFAULT_SPEED);
        assert_eq!(params.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn allowed_key_envs_empty_by_default() {
        assert!(parse_allowed_key_envs("").is_empty());
    }

    #[test]
    fn allowed_key_envs_parses_comma_list() {
        let allowed = parse_allowed_key_envs("OPENAI_API_KEY, ELEVENLABS_API_KEY ,,");
        assert!(is_allowed_key_env("OPENAI_API_KEY", &allowed));
        assert!(is_allowed_key_env("ELEVENLABS_API_KEY", &allowed));
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
    fn voices_request_accepts_sherpa_and_openai() {
        assert_eq!(
            parse_voices_request(br#"{"provider":"sherpa"}"#).unwrap(),
            Provider::Sherpa
        );
        assert_eq!(
            parse_voices_request(br#"{"provider":"openai"}"#).unwrap(),
            Provider::OpenAi
        );
    }

    #[test]
    fn voices_request_rejects_elevenlabs() {
        let err = parse_voices_request(br#"{"provider":"elevenlabs"}"#).unwrap_err();
        assert!(err.contains("per-account"), "error was: {err}");
    }

    #[test]
    fn speak_accepts_minimal_request() {
        let params = parse_speak_request(
            br#"{"provider":"sherpa","text":"hi","voice":"af_heart","target":"phone-1.speaker"}"#,
        )
        .unwrap();
        assert_eq!(params.text, "hi");
        assert_eq!(params.voice, "af_heart");
        assert_eq!(params.target, "phone-1.speaker");
        assert_eq!(params.stream_id, SPEAK_DEFAULT_STREAM_ID);
        assert_eq!(params.sample_rate_hz, SPEAK_DEFAULT_SAMPLE_RATE);
        assert_eq!(params.bitrate, SPEAK_DEFAULT_BITRATE);
        assert_eq!(params.speed, DEFAULT_SPEED);
    }

    #[test]
    fn speak_accepts_overrides() {
        let params = parse_speak_request(
            br#"{"provider":"sherpa","text":"hi","voice":"sid:3","target":"speaker","stream_id":7,"sample_rate_hz":16000,"bitrate":16000,"speed":1.5}"#,
        )
        .unwrap();
        assert_eq!(params.stream_id, 7);
        assert_eq!(params.sample_rate_hz, 16_000);
        assert_eq!(params.bitrate, 16_000);
        assert_eq!(params.speed, 1.5);
    }

    #[test]
    fn speak_rejects_cloud_providers() {
        let err = parse_speak_request(
            br#"{"provider":"openai","text":"hi","voice":"alloy","api_key_env":"OPENAI_API_KEY","target":"spk"}"#,
        )
        .unwrap_err();
        assert!(err.contains("sherpa-only"), "error was: {err}");
    }

    #[test]
    fn speak_requires_target() {
        let mut body = serde_json::json!({
            "provider": "sherpa", "text": "hi", "voice": "af_heart",
        });
        let err = parse_speak_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("target"), "error was: {err}");
        body["target"] = "".into();
        let err = parse_speak_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("target"), "error was: {err}");
    }

    #[test]
    fn speak_validates_text_and_voice_like_synthesize() {
        let mut body = serde_json::json!({
            "provider": "sherpa", "voice": "af_heart", "target": "spk",
        });
        let err = parse_speak_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("text"), "error was: {err}");
        body["text"] = "x".repeat(MAX_TEXT_CHARS + 1).into();
        let err = parse_speak_request(body.to_string().as_bytes()).unwrap_err();
        assert!(err.contains("max length"), "error was: {err}");
    }
}
