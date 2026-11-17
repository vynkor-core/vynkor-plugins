//! Streaming listen state: accumulate inbound `AudioStreamChunk` PCM into
//! per-stream buffers, then transcribe on `stt_listen_stop`.
//!
//! The kernel routes `AudioStreamChunk` envelopes (codec `PCM_S16LE`) from
//! a mic-capable peer to the `stt` plugin like any other message. This
//! module owns the accumulation: one buffer per `stream_id`, downmixed to
//! mono, rate-checked so a caller can't splice mismatched streams together.
//!
//! When the operator enables the energy VAD ([`crate::vad`]), every chunk
//! also advances the stream's [`VadState`] and [`ListenStream::push`]
//! reports whether the chunk crossed a speech boundary — the serve loop
//! turns those crossings into best-effort `stt_speech_started` /
//! `stt_speech_ended` events.

use crate::vad::{VadConfig, VadState, VadTransition};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// What one accumulated chunk did to the stream's voice-activity state.
/// Both fields are inert when the VAD is disabled (the common case).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChunkVad {
    /// The chunk completed the loud-chunk streak and opened an utterance.
    pub speech_started: bool,
    /// The utterance closed after this much measured speech (ms). `None`
    /// while talking, during the silence hangover, or on a too-short blip.
    pub speech_ended_ms: Option<u32>,
}

impl ChunkVad {
    fn from_transition(t: VadTransition) -> Self {
        match t {
            VadTransition::None => Self::default(),
            VadTransition::SpeechStarted => Self {
                speech_started: true,
                speech_ended_ms: None,
            },
            VadTransition::SpeechEnded { speech_ms } => Self {
                speech_started: false,
                speech_ended_ms: Some(speech_ms),
            },
        }
    }
}

/// One accumulating stream. `rate_hz`/`channels` come from the first chunk
/// (the `stt_listen_start` action or the negotiation chunk) and are locked
/// for the stream's lifetime.
#[derive(Debug, Clone)]
pub struct ListenStream {
    pub stream_id: u32,
    pub rate_hz: u32,
    pub channels: u16,
    /// Caller-declared language hint, applied at transcription time.
    pub language: Option<String>,
    pcm: Vec<i16>,
    /// Voice-activity hysteresis state; advanced only when the operator's
    /// [`VadConfig`] is enabled, otherwise permanently inert.
    vad: VadState,
}

impl ListenStream {
    /// Accumulate one PCM chunk. The chunk's rate must match this stream's
    /// (a mismatch means a spliced or mislabeled stream — reject loudly).
    /// Returns what the chunk did to the voice-activity state (inert unless
    /// the operator's [`VadConfig`] is enabled).
    pub fn push(
        &mut self,
        chunk_rate_hz: u32,
        chunk_channels: u16,
        data: &[u8],
        vad_cfg: &VadConfig,
    ) -> Result<ChunkVad, String> {
        if chunk_rate_hz != self.rate_hz {
            return Err(format!(
                "stream {} rate mismatch: negotiated {} Hz, chunk at {} Hz",
                self.stream_id, self.rate_hz, chunk_rate_hz
            ));
        }
        if !data.len().is_multiple_of(2) {
            return Err(format!(
                "stream {} chunk has odd byte length {} (expected 16-bit samples)",
                self.stream_id,
                data.len()
            ));
        }
        let samples: Vec<i16> = data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        let mono = downmix(&samples, chunk_channels.max(1) as usize);
        let transition = self.vad.feed(vad_cfg, &mono, self.rate_hz);
        self.pcm.extend(mono);
        Ok(ChunkVad::from_transition(transition))
    }

    pub fn is_empty(&self) -> bool {
        self.pcm.is_empty()
    }

    pub fn take_pcm(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.pcm)
    }
}

/// Downmix interleaved multi-channel samples to mono by averaging (shared
/// with the sherpa decoder; kept local so the buffer module stays
/// self-contained).
fn downmix(samples: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let frames = samples.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    for frame in samples.chunks_exact(channels) {
        let sum: i32 = frame.iter().map(|&s| s as i32).sum();
        mono.push((sum / channels as i32) as i16);
    }
    mono
}

/// All active listen streams, keyed by `stream_id`. The serve loop is
/// single-threaded, so a plain `Mutex<HashMap>` is enough; the lock is
/// held only across one chunk append.
static STREAMS: LazyLock<Mutex<HashMap<u32, ListenStream>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Start (or reset) a listen stream. Errors if a stream with this id is
/// already accumulating — a caller must `stop` first.
pub fn start(
    stream_id: u32,
    rate_hz: u32,
    channels: u16,
    language: Option<String>,
) -> Result<(), String> {
    if rate_hz == 0 {
        return Err("listen start requires sample_rate_hz".to_string());
    }
    if channels == 0 {
        return Err("listen start requires num_channels".to_string());
    }
    let mut streams = STREAMS.lock().unwrap();
    if streams.contains_key(&stream_id) {
        return Err(format!("listen stream {stream_id} is already active"));
    }
    streams.insert(
        stream_id,
        ListenStream {
            stream_id,
            rate_hz,
            channels,
            language,
            pcm: Vec::new(),
            vad: VadState::new(),
        },
    );
    Ok(())
}

/// Append one inbound chunk to its stream. Errors on unknown stream id or
/// a rate mismatch (see [`ListenStream::push`]).
pub fn push(
    stream_id: u32,
    chunk_rate_hz: u32,
    chunk_channels: u16,
    data: &[u8],
    vad_cfg: &VadConfig,
) -> Result<ChunkVad, String> {
    let mut streams = STREAMS.lock().unwrap();
    let stream = streams
        .get_mut(&stream_id)
        .ok_or_else(|| format!("no active listen stream {stream_id}"))?;
    stream.push(chunk_rate_hz, chunk_channels, data, vad_cfg)
}

/// Take and remove a stream for transcription. The caller owns the PCM now;
/// a subsequent `stt_listen_start` with the same id starts fresh.
pub fn take(stream_id: u32) -> Result<ListenStream, String> {
    STREAMS
        .lock()
        .unwrap()
        .remove(&stream_id)
        .ok_or_else(|| format!("no active listen stream {stream_id}"))
}

/// Drop a stream without transcribing (e.g. a stale `end_of_stream` after a
/// manual stop). Errors only if the stream is missing.
pub fn discard(stream_id: u32) -> Result<(), String> {
    STREAMS
        .lock()
        .unwrap()
        .remove(&stream_id)
        .map(|_| ())
        .ok_or_else(|| format!("no active listen stream {stream_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vad::VadConfig;

    fn inert() -> VadConfig {
        VadConfig::default()
    }

    #[test]
    fn start_requires_metadata() {
        let err = start(1, 0, 1, None).unwrap_err();
        assert!(err.contains("sample_rate_hz"), "error was: {err}");
        let err = start(1, 16000, 0, None).unwrap_err();
        assert!(err.contains("num_channels"), "error was: {err}");
    }

    #[test]
    fn start_rejects_duplicate_stream() {
        start(1, 16000, 1, None).unwrap();
        let err = start(1, 16000, 1, None).unwrap_err();
        assert!(err.contains("already active"), "error was: {err}");
        take(1).unwrap();
    }

    #[test]
    fn push_accumulates_and_downmixes() {
        start(2, 16000, 2, Some("en".into())).unwrap();
        let stereo = [10i16, 20, 30, 40]; // 2 frames of stereo
        let mut bytes = Vec::new();
        for s in stereo {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        push(2, 16000, 2, &bytes, &inert()).unwrap();
        let mut stream = take(2).unwrap();
        assert_eq!(stream.rate_hz, 16000);
        assert_eq!(stream.language.as_deref(), Some("en"));
        let pcm = stream.take_pcm();
        assert_eq!(pcm, vec![15, 35], "stereo frames must average to mono");
    }

    #[test]
    fn push_reports_vad_transitions_when_enabled() {
        start(7, 16000, 1, None).unwrap();
        let cfg = crate::vad::VadConfig {
            enabled: true,
            rms_threshold: 500,
            silence_ms: 200,
            min_speech_ms: 100,
        };
        let loud = loud_pcm(1600);
        let quiet = quiet_pcm(1600);
        assert!(!push(7, 16000, 1, &loud, &cfg).unwrap().speech_started);
        let started = push(7, 16000, 1, &loud, &cfg).unwrap();
        assert!(started.speech_started);
        // Two quiet chunks (200 ms) close the utterance.
        push(7, 16000, 1, &quiet, &cfg).unwrap();
        let ended = push(7, 16000, 1, &quiet, &cfg).unwrap();
        assert!(ended.speech_ended_ms.is_some(), "expected an ending");
        take(7).unwrap();
    }

    fn loud_pcm(n: usize) -> Vec<u8> {
        pcm_bytes(&vec![12_000i16; n])
    }

    fn quiet_pcm(n: usize) -> Vec<u8> {
        pcm_bytes(&vec![3i16; n])
    }

    fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn push_rejects_rate_mismatch() {
        start(3, 16000, 1, None).unwrap();
        let err = push(3, 24000, 1, &[0u8; 4], &inert()).unwrap_err();
        assert!(err.contains("rate mismatch"), "error was: {err}");
        discard(3).unwrap();
    }

    #[test]
    fn push_rejects_odd_length() {
        start(4, 16000, 1, None).unwrap();
        let err = push(4, 16000, 1, &[0u8; 3], &inert()).unwrap_err();
        assert!(err.contains("odd"), "error was: {err}");
        discard(4).unwrap();
    }

    #[test]
    fn push_unknown_stream_errors() {
        let err = push(99, 16000, 1, &[0u8; 4], &inert()).unwrap_err();
        assert!(err.contains("no active"), "error was: {err}");
    }

    #[test]
    fn take_consumes_and_discard_errors() {
        start(5, 16000, 1, None).unwrap();
        take(5).unwrap();
        let err = take(5).unwrap_err();
        assert!(err.contains("no active"), "error was: {err}");
        start(6, 16000, 1, None).unwrap();
        discard(6).unwrap();
        let err = discard(6).unwrap_err();
        assert!(err.contains("no active"), "error was: {err}");
    }
}
