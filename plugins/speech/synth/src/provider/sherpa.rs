//! Local TTS provider: sherpa-onnx, in-process, no HTTP.
//!
//! Loads an ONNX model from disk (lazily on the first synthesize request,
//! cached for the process lifetime) and runs inference via the
//! `sherpa-onnx` crate. Two model families, selected by the operator with
//! `TTS_PLUGIN_LOCAL_MODEL_TYPE`:
//!
//!   - `kokoro` — Kokoro-82M. Files in `TTS_PLUGIN_LOCAL_MODEL_DIR`:
//!     `model.onnx`, `voices.bin`, `tokens.txt`, `espeak-ng-data/`
//!     (directory). Optional for non-English text: `lexicon-*.txt` files
//!     and a `dict/` directory (auto-detected; the default install for
//!     English text needs none of them).
//!   - `piper` — Piper voices (VITS). Files: `model.onnx`, `tokens.txt`,
//!     `espeak-ng-data/` (directory). Voices from
//!     `rhasspy/piper-voices` work as-is; extract the matching
//!     `espeak-ng-data` next to the model.
//!
//! Voice selection: Kokoro voice names (`af_heart`, ...) are mapped to
//! sids via the official voices table; `sid:N` escapes to a raw index for
//! either family; Piper models are single-speaker so any name maps to 0.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig,
    OfflineTtsModelConfig, OfflineTtsVitsModelConfig,
};

use crate::provider::{
    encode_base64, f32_to_wav, kokoro_voice_to_sid, AudioResult, VoiceInfo, KOKORO_VOICES,
};
use crate::request::{AudioFormat, SynthesizeParams};

pub const ENV_MODEL_DIR: &str = "TTS_PLUGIN_LOCAL_MODEL_DIR";
pub const ENV_MODEL_TYPE: &str = "TTS_PLUGIN_LOCAL_MODEL_TYPE";
pub const ENV_NUM_THREADS: &str = "TTS_PLUGIN_LOCAL_NUM_THREADS";
pub const ENV_KOKORO_LEXICON: &str = "TTS_PLUGIN_KOKORO_LEXICON";

const DEFAULT_NUM_THREADS: i32 = 2;

/// The loaded engine plus the model family it was built for (voice
/// resolution differs between kokoro and piper).
struct LoadedEngine {
    tts: Arc<OfflineTts>,
    model_type: String,
}

static ENGINE: OnceLock<Result<LoadedEngine, String>> = OnceLock::new();

/// Lazy, process-lifetime model load. A failed load is cached too — a bad
/// operator config won't be retried (and error-flooded) on every request;
/// it fails fast and consistently until the process restarts.
fn engine() -> Result<&'static LoadedEngine, String> {
    match ENGINE.get_or_init(load_engine) {
        Ok(loaded) => Ok(loaded),
        Err(e) => Err(e.clone()),
    }
}

fn load_engine() -> Result<LoadedEngine, String> {
    let model_dir = std::env::var(ENV_MODEL_DIR)
        .map_err(|_| format!("{ENV_MODEL_DIR} is not set (local provider needs a model dir)"))?;
    let dir = Path::new(&model_dir);
    if !dir.is_dir() {
        return Err(format!("{ENV_MODEL_DIR}={model_dir} is not a directory"));
    }

    let model_type = std::env::var(ENV_MODEL_TYPE).map_err(|_| {
        format!("{ENV_MODEL_TYPE} is not set (local provider needs 'kokoro' or 'piper')")
    })?;
    let model_type = model_type_str(&model_type)?;

    let num_threads = match std::env::var(ENV_NUM_THREADS) {
        Ok(raw) => raw
            .parse::<i32>()
            .map_err(|_| format!("{ENV_NUM_THREADS}={raw} is not an integer"))?,
        Err(_) => DEFAULT_NUM_THREADS,
    };

    let config = match model_type {
        "kokoro" => build_kokoro_config(dir, num_threads)?,
        "piper" => build_piper_config(dir, num_threads)?,
        _ => unreachable!("model_type_str validated"),
    };

    let tts = OfflineTts::create(&config).ok_or_else(|| {
        format!(
            "sherpa-onnx failed to load model from {ENV_MODEL_DIR}={model_dir} \
             (check the file layout for model type '{model_type}', see README.md)"
        )
    })?;

    Ok(LoadedEngine {
        tts: Arc::new(tts),
        model_type: model_type.to_string(),
    })
}

fn model_type_str(raw: &str) -> Result<&'static str, String> {
    match raw {
        "kokoro" => Ok("kokoro"),
        "piper" => Ok("piper"),
        other => Err(format!(
            "{ENV_MODEL_TYPE}={other} is unsupported (use 'kokoro' or 'piper')"
        )),
    }
}

/// Files relative to the model dir that every family needs.
fn require_file(dir: &Path, name: &str) -> Result<String, String> {
    let path = dir.join(name);
    if !path.is_file() {
        return Err(format!(
            "missing required model file: {} (expected at {ENV_MODEL_DIR}/{name})",
            path.display()
        ));
    }
    Ok(path.to_string_lossy().into_owned())
}

/// Required model asset that may be a file or a directory (e.g.
/// `espeak-ng-data/`), unlike `require_file`.
fn require_path(dir: &Path, name: &str) -> Result<String, String> {
    let path = dir.join(name);
    if !path.exists() {
        return Err(format!(
            "missing required model asset: {} (expected at {ENV_MODEL_DIR}/{name})",
            path.display()
        ));
    }
    Ok(path.to_string_lossy().into_owned())
}

/// Auto-detect the Kokoro lexicon list: every `lexicon-*.txt` present in
/// the model dir, joined the way sherpa-onnx expects. An explicit
/// `TTS_PLUGIN_KOKORO_LEXICON` override wins when set.
fn kokoro_lexicon(dir: &Path) -> String {
    if let Ok(override_list) = std::env::var(ENV_KOKORO_LEXICON) {
        if !override_list.trim().is_empty() {
            return override_list;
        }
    }
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_name().to_string_lossy().starts_with("lexicon-") && e.path().is_file()
                })
                .map(|e| e.path().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found.join(",")
}

fn build_kokoro_config(dir: &Path, num_threads: i32) -> Result<OfflineTtsConfig, String> {
    let dict_dir = dir.join("dict");
    Ok(OfflineTtsConfig {
        model: OfflineTtsModelConfig {
            kokoro: OfflineTtsKokoroModelConfig {
                model: Some(require_file(dir, "model.onnx")?),
                voices: Some(require_file(dir, "voices.bin")?),
                tokens: Some(require_file(dir, "tokens.txt")?),
                data_dir: Some(require_path(dir, "espeak-ng-data")?),
                // Optional assets: only set when present, so an English-only
                // install works with the four files above alone.
                dict_dir: dict_dir
                    .is_dir()
                    .then(|| dict_dir.to_string_lossy().into_owned()),
                lexicon: {
                    let l = kokoro_lexicon(dir);
                    (!l.is_empty()).then_some(l)
                },
                ..Default::default()
            },
            num_threads,
            ..Default::default()
        },
        ..Default::default()
    })
}

fn build_piper_config(dir: &Path, num_threads: i32) -> Result<OfflineTtsConfig, String> {
    Ok(OfflineTtsConfig {
        model: OfflineTtsModelConfig {
            vits: OfflineTtsVitsModelConfig {
                model: Some(require_file(dir, "model.onnx")?),
                tokens: Some(require_file(dir, "tokens.txt")?),
                data_dir: Some(require_path(dir, "espeak-ng-data")?),
                ..Default::default()
            },
            num_threads,
            ..Default::default()
        },
        ..Default::default()
    })
}

/// Map a caller's `voice` string to a model sid.
///
///   - kokoro: official voice name (`af_heart`) or `sid:N`.
///   - piper: `sid:N` for multi-speaker models; any name maps to 0 for the
///     usual single-speaker voices.
fn resolve_sid(model_type: &str, voice: &str, num_speakers: i32) -> Result<i32, String> {
    let sid = match model_type {
        "kokoro" => kokoro_voice_to_sid(voice)?,
        _ => {
            if let Some(n) = voice.strip_prefix("sid:") {
                n.parse::<i32>()
                    .map_err(|_| format!("invalid sid: {voice}"))?
            } else if num_speakers <= 1 {
                0
            } else {
                return Err(format!(
                    "this piper model has {num_speakers} speakers; use voice \"sid:0\"..\"sid:{}\"",
                    num_speakers - 1
                ));
            }
        }
    };
    if sid < 0 || sid >= num_speakers {
        return Err(format!(
            "voice '{voice}' resolves to sid {sid}, but the model has only \
             {num_speakers} speaker(s)"
        ));
    }
    Ok(sid)
}

/// Synthesize `params.text` with the local engine. CPU-bound and blocking;
/// callers (the serve loop) run one request at a time, so this is fine.
pub fn synthesize(params: &SynthesizeParams) -> Result<AudioResult, String> {
    let (samples, sample_rate) = synthesize_samples(&params.text, &params.voice, params.speed)?;
    // sherpa-onnx TTS output is always mono.
    let channels: u16 = 1;
    let duration_seconds = samples.len() as f32 / (sample_rate * channels as u32) as f32;

    let (bytes, format) = match params.format {
        AudioFormat::Wav => (
            f32_to_wav(&samples, sample_rate, channels),
            "wav".to_string(),
        ),
        AudioFormat::Pcm => {
            let mut raw = Vec::with_capacity(samples.len() * 2);
            for &s in &samples {
                let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                raw.extend_from_slice(&v.to_le_bytes());
            }
            (raw, "pcm".to_string())
        }
        AudioFormat::Mp3 => {
            let pcm: Vec<i16> = samples
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            let bytes = crate::provider::mp3::encode_pcm(&pcm, sample_rate, channels as u8)?;
            (bytes, "mp3".to_string())
        }
        other => unreachable!(
            "sherpa only produces wav|pcm|mp3 (requested {})",
            other.as_str()
        ),
    };

    Ok(AudioResult {
        format,
        sample_rate_hz: sample_rate,
        num_channels: channels as u8,
        duration_seconds,
        audio_base64: encode_base64(&bytes),
    })
}

/// Synthesize to raw mono `f32` samples plus the model's sample rate —
/// the building block both [`synthesize`] (wav/pcm packaging) and the
/// `tts_speak` streaming path (Opus encode) share.
pub fn synthesize_samples(text: &str, voice: &str, speed: f32) -> Result<(Vec<f32>, u32), String> {
    let loaded = engine()?;
    let num_speakers = loaded.tts.num_speakers();
    let sid = resolve_sid(&loaded.model_type, voice, num_speakers)?;

    let gen = GenerationConfig {
        sid,
        speed,
        ..Default::default()
    };
    let audio = loaded
        .tts
        .generate_with_config(text, &gen, None::<fn(&[f32], f32) -> bool>)
        .ok_or_else(|| "synthesis failed: sherpa-onnx returned no audio".to_string())?;

    let sample_rate = audio.sample_rate().max(1) as u32;
    Ok((audio.samples().to_vec(), sample_rate))
}

/// List the voices the loaded model exposes.
pub fn voices() -> Result<Vec<VoiceInfo>, String> {
    let loaded = engine()?;
    let n = loaded.tts.num_speakers();
    let mut out: Vec<VoiceInfo> = Vec::new();

    if loaded.model_type == "kokoro" {
        for (name, sid) in KOKORO_VOICES {
            if sid < n {
                out.push(VoiceInfo {
                    id: name.to_string(),
                    name: name.to_string(),
                });
            }
        }
    }
    for sid in 0..n {
        let id = format!("sid:{sid}");
        if !out.iter().any(|v| v.id == id) {
            out.push(VoiceInfo {
                id,
                name: format!("speaker {sid}"),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_type_validates() {
        assert_eq!(model_type_str("kokoro").unwrap(), "kokoro");
        assert_eq!(model_type_str("piper").unwrap(), "piper");
        assert!(model_type_str("wavenet").is_err());
    }

    #[test]
    fn resolve_sid_kokoro_name_and_escape() {
        assert_eq!(resolve_sid("kokoro", "af_heart", 26).unwrap(), 0);
        assert_eq!(resolve_sid("kokoro", "sid:9", 26).unwrap(), 9);
    }

    #[test]
    fn resolve_sid_kokoro_rejects_out_of_range() {
        let err = resolve_sid("kokoro", "ff_siwis", 10).unwrap_err();
        assert!(err.contains("speaker"), "error was: {err}");
    }

    #[test]
    fn resolve_sid_piper_single_speaker_accepts_any_name() {
        assert_eq!(resolve_sid("piper", "whatever", 1).unwrap(), 0);
        assert_eq!(resolve_sid("piper", "0", 1).unwrap(), 0);
    }

    #[test]
    fn resolve_sid_piper_multi_speaker_requires_sid() {
        assert_eq!(resolve_sid("piper", "sid:2", 4).unwrap(), 2);
        let err = resolve_sid("piper", "alice", 4).unwrap_err();
        assert!(err.contains("sid:0"), "error was: {err}");
    }

    #[test]
    fn require_file_reports_missing() {
        let err = require_file(Path::new("/nonexistent-dir"), "model.onnx").unwrap_err();
        assert!(
            err.contains("missing required model file"),
            "error was: {err}"
        );
    }

    #[test]
    fn require_path_accepts_directories() {
        let dir =
            std::env::temp_dir().join(format!("tts-require-path-test-{}", std::process::id()));
        let sub = dir.join("espeak-ng-data");
        let _ = std::fs::create_dir_all(&sub);
        let got = require_path(&dir, "espeak-ng-data").unwrap();
        assert!(got.ends_with("espeak-ng-data"), "path was: {got}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kokoro_lexicon_detects_existing_files() {
        let dir = std::env::temp_dir().join(format!("tts-lexicon-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("lexicon-us-en.txt"), "x").unwrap();
        std::fs::write(dir.join("lexicon-zh.txt"), "x").unwrap();
        std::fs::write(dir.join("other.txt"), "x").unwrap();
        std::env::remove_var(ENV_KOKORO_LEXICON);
        let list = kokoro_lexicon(&dir);
        assert!(list.contains("lexicon-us-en.txt"), "list was: {list}");
        assert!(list.contains("lexicon-zh.txt"), "list was: {list}");
        assert!(!list.contains("other.txt"), "list was: {list}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kokoro_lexicon_empty_when_none() {
        let dir = std::env::temp_dir().join(format!("tts-lexicon-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::remove_var(ENV_KOKORO_LEXICON);
        assert_eq!(kokoro_lexicon(&dir), "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
