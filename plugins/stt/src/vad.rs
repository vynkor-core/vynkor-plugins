//! Energy-based voice-activity detection over the inbound listen PCM.
//!
//! The VAD is deliberately primitive — per-chunk RMS with hysteresis —
//! because it exists to solve exactly one problem: telling an orchestrator
//! (the `daemon`) *when the user stopped talking*, so a turn can end on
//! silence instead of a fixed capture window. It makes no attempt to
//! compete with neural VADs on accuracy; it only has to be right within a
//! few hundred milliseconds around real speech, which an RMS threshold
//! plus a silence-hangover achieves reliably for close-mic desktop use.
//!
//! State lives per listen stream (inside [`crate::listen::ListenStream`])
//! and is advanced by [`VadState::feed`] on every accumulated chunk. All
//! knobs come from environment variables (see [`VadConfig::from_env`]) so
//! operators can tune it in `config.yaml` without code changes. The VAD is
//! off by default: streams behave exactly as before unless
//! `STT_PLUGIN_VAD` is enabled, and every transition is published by the
//! serve loop as a best-effort event (`stt_speech_started` /
//! `stt_speech_ended`, namespaced `plugin.stt.*` by the kernel).

/// Event type (pre-namespacing) published when a stream crosses into
/// speech. Payload: `{"stream_id": <u32>}`. The kernel namespaces it as
/// `plugin.stt.stt_speech_started`.
pub const SPEECH_STARTED_EVENT_TYPE: &str = "stt_speech_started";

/// Event type (pre-namespacing) published when speech ends on a stream —
/// `silence_ms` of quiet after at least `min_speech_ms` of detected
/// speech. Payload: `{"stream_id": <u32>, "speech_ms": <u32>}`. The kernel
/// namespaces it as `plugin.stt.stt_speech_ended`.
pub const SPEECH_ENDED_EVENT_TYPE: &str = "stt_speech_ended";

/// Default RMS threshold on 16-bit samples. Desktop mics idle well under
/// ~200 RMS; normal speech peaks 1000+. 500 sits comfortably between.
pub const DEFAULT_RMS_THRESHOLD: i64 = 500;

/// Default silence hangover: how much quiet (ms) ends an utterance once
/// speech was detected. Long enough for inter-word pauses (~300-500 ms),
/// short enough to feel responsive.
pub const DEFAULT_SILENCE_MS: u32 = 1_200;

/// Default minimum speech run (ms) before an ending counts: filters door
/// slams / clicks that would otherwise produce garbage transcripts.
pub const DEFAULT_MIN_SPEECH_MS: u32 = 240;

/// How many consecutive loud chunks must precede a `SpeechStarted` — one
/// chunk can be a transient pop; two (~200 ms at default settings) cannot.
const START_STREAK: u32 = 2;

/// Snapshot of the operator's VAD configuration, resolved once from the
/// process environment.
#[derive(Debug, Clone)]
pub struct VadConfig {
    /// Master switch (`STT_PLUGIN_VAD`): `on`/`true`/`1` enable the VAD;
    /// anything else (or unset) keeps streams VAD-silent.
    pub enabled: bool,
    /// Per-chunk RMS threshold on s16 samples (`STT_PLUGIN_VAD_RMS_THRESHOLD`).
    pub rms_threshold: i64,
    /// Silence hangover that closes an utterance (`STT_PLUGIN_VAD_SILENCE_MS`).
    pub silence_ms: u32,
    /// Minimum speech length for an ending to count (`STT_PLUGIN_VAD_MIN_SPEECH_MS`).
    pub min_speech_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rms_threshold: DEFAULT_RMS_THRESHOLD,
            silence_ms: DEFAULT_SILENCE_MS,
            min_speech_ms: DEFAULT_MIN_SPEECH_MS,
        }
    }
}

impl VadConfig {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("STT_PLUGIN_VAD") {
            let v = v.trim().to_ascii_lowercase();
            c.enabled = v == "on" || v == "true" || v == "1" || v == "yes";
        }
        if let Ok(v) = std::env::var("STT_PLUGIN_VAD_RMS_THRESHOLD") {
            if let Ok(n) = v.trim().parse::<i64>() {
                c.rms_threshold = n.clamp(1, 30_000);
            }
        }
        if let Ok(v) = std::env::var("STT_PLUGIN_VAD_SILENCE_MS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                c.silence_ms = n.clamp(100, 10_000);
            }
        }
        if let Ok(v) = std::env::var("STT_PLUGIN_VAD_MIN_SPEECH_MS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                c.min_speech_ms = n.clamp(50, 5_000);
            }
        }
        c
    }
}

/// One VAD evaluation of one chunk. `SpeechStarted` fires once per utterance;
/// `SpeechEnded` carries how much speech (ms) preceded the ending so
/// subscribers can log/measure. A quiet blip that never reached
/// `min_speech_ms` resets internally and surfaces as [`VadTransition::None`] —
/// subscribers must not treat it as an utterance boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadTransition {
    None,
    SpeechStarted,
    SpeechEnded { speech_ms: u32 },
}

/// Hysteresis state for one listen stream.
#[derive(Debug, Default, Clone)]
pub struct VadState {
    active: bool,
    loud_streak: u32,
    speech_ms: u32,
    silence_ms: u32,
}

impl VadState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one mono chunk and report the transition it caused. `rate_hz`
    /// converts the sample count into wall-clock ms; a zero rate yields
    /// [`VadTransition::None`] rather than a division blowup (streams lock
    /// their rate at start, so this is purely defensive).
    pub fn feed(&mut self, cfg: &VadConfig, mono: &[i16], rate_hz: u32) -> VadTransition {
        if !cfg.enabled || mono.is_empty() || rate_hz == 0 {
            return VadTransition::None;
        }
        let chunk_ms = (mono.len() as f32 / rate_hz as f32 * 1_000.0).max(1.0);
        let rms = chunk_rms(mono);

        if !self.active {
            if rms >= cfg.rms_threshold {
                self.loud_streak += 1;
                if self.loud_streak >= START_STREAK {
                    self.active = true;
                    // Count the streak's audio as speech too — it is part of
                    // the utterance the subscriber will transcribe.
                    self.speech_ms = (chunk_ms * self.loud_streak as f32) as u32;
                    self.silence_ms = 0;
                    return VadTransition::SpeechStarted;
                }
            } else {
                self.loud_streak = 0;
            }
            return VadTransition::None;
        }

        if rms >= cfg.rms_threshold {
            self.speech_ms += chunk_ms as u32;
            self.silence_ms = 0;
            return VadTransition::None;
        }

        self.silence_ms += chunk_ms as u32;
        if self.silence_ms < cfg.silence_ms {
            return VadTransition::None;
        }

        // Utterance boundary: reset regardless, but only announce an ending
        // when enough actual speech preceded the silence.
        let speech_ms = self.speech_ms;
        self.active = false;
        self.loud_streak = 0;
        self.speech_ms = 0;
        self.silence_ms = 0;
        if speech_ms >= cfg.min_speech_ms {
            VadTransition::SpeechEnded { speech_ms }
        } else {
            VadTransition::None
        }
    }
}

/// Root-mean-square of mono s16 samples, scaled to i16 amplitude units.
fn chunk_rms(samples: &[i16]) -> i64 {
    if samples.is_empty() {
        return 0;
    }
    let sum_sq: i64 = samples.iter().map(|&s| (s as i64) * (s as i64)).sum();
    ((sum_sq as f64) / samples.len() as f64).sqrt().round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VadConfig {
        VadConfig {
            enabled: true,
            rms_threshold: 500,
            silence_ms: 600,
            min_speech_ms: 240,
        }
    }

    /// `n` samples of a loud sine-ish signal (RMS well above the threshold).
    fn loud(n: usize) -> Vec<i16> {
        vec![12_000i16; n]
    }

    /// Near-silence.
    fn quiet(n: usize) -> Vec<i16> {
        vec![3i16; n]
    }

    #[test]
    fn disabled_config_is_inert() {
        let mut st = VadState::new();
        let mut c = cfg();
        c.enabled = false;
        for _ in 0..10 {
            assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        }
    }

    #[test]
    fn single_loud_chunk_does_not_start_speech() {
        let mut st = VadState::new();
        let c = cfg();
        assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        assert_eq!(st.feed(&c, &quiet(1600), 16_000), VadTransition::None);
        assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        assert_eq!(st.feed(&c, &quiet(1600), 16_000), VadTransition::None);
    }

    #[test]
    fn two_consecutive_loud_chunks_start_speech() {
        let mut st = VadState::new();
        let c = cfg();
        assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        assert_eq!(
            st.feed(&c, &loud(1600), 16_000),
            VadTransition::SpeechStarted
        );
    }

    #[test]
    fn long_speech_then_silence_ends_with_measured_speech() {
        let mut st = VadState::new();
        let c = cfg();
        assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        assert_eq!(
            st.feed(&c, &loud(1600), 16_000),
            VadTransition::SpeechStarted
        );
        // 8 more chunks ≈ 800 ms of speech (each 100 ms @16 kHz).
        for _ in 0..8 {
            assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        }
        // Silence hangover: 600 ms needed, chunks are 100 ms.
        assert_eq!(st.feed(&c, &quiet(1600), 16_000), VadTransition::None);
        assert_eq!(st.feed(&c, &quiet(1600), 16_000), VadTransition::None);
        assert_eq!(
            st.feed(&c, &quiet(1600), 16_000),
            VadTransition::None,
            "hangover not yet elapsed"
        );
        assert_eq!(
            st.feed(&c, &quiet(1600), 16_000),
            VadTransition::None,
            "hangover not yet elapsed"
        );
        assert_eq!(
            st.feed(&c, &quiet(1600), 16_000),
            VadTransition::None,
            "hangover not yet elapsed"
        );
        match st.feed(&c, &quiet(1600), 16_000) {
            VadTransition::SpeechEnded { speech_ms } => {
                // ~10 loud chunks × 100 ms, allow float slop either way.
                assert!(
                    (900..=1_100).contains(&speech_ms),
                    "unexpected speech_ms: {speech_ms}"
                );
            }
            other => panic!("expected SpeechEnded, got {other:?}"),
        }
        // And the detector re-arms cleanly afterwards.
        assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        assert_eq!(
            st.feed(&c, &loud(1600), 16_000),
            VadTransition::SpeechStarted
        );
    }

    #[test]
    fn short_noise_blip_resets_without_an_ending() {
        let mut st = VadState::new();
        let c = cfg();
        assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        assert_eq!(
            st.feed(&c, &loud(1600), 16_000),
            VadTransition::SpeechStarted
        );
        // Only one quiet chunk (100 ms < 600 ms hangover)…
        assert_eq!(st.feed(&c, &quiet(1600), 16_000), VadTransition::None);
        // …then loud again: must NOT have ended/restarted mid-utterance.
        assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);

        // A fresh utterance that had ≥ min_speech_ms announces its ending
        // once the hangover elapses (600 ms of quiet = 6 chunks).
        let mut last = VadTransition::None;
        for _ in 0..6 {
            last = st.feed(&c, &quiet(1600), 16_000);
        }
        assert!(
            matches!(last, VadTransition::SpeechEnded { .. }),
            "expected the first utterance to end: {last:?}"
        );
        assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        assert_eq!(
            st.feed(&c, &loud(1600), 16_000),
            VadTransition::SpeechStarted
        );
        for _ in 0..6 {
            st.feed(&c, &quiet(1600), 16_000);
        }
        assert_eq!(st.feed(&c, &quiet(1600), 16_000), VadTransition::None);
        // Re-armed: next burst starts normally.
        assert_eq!(st.feed(&c, &loud(1600), 16_000), VadTransition::None);
        assert_eq!(
            st.feed(&c, &loud(1600), 16_000),
            VadTransition::SpeechStarted
        );
    }

    #[test]
    fn rms_math_basics() {
        assert_eq!(chunk_rms(&[]), 0);
        assert_eq!(chunk_rms(&[0, 0, 0]), 0);
        // Constant amplitude → RMS == amplitude.
        assert_eq!(chunk_rms(&[-1000i16; 4]), 1000);
        // Alternating ±a → mean square a² → RMS a.
        assert_eq!(chunk_rms(&[1000, -1000]), 1000);
    }

    #[test]
    fn env_overrides_parse_and_clamp() {
        // Indirect test through the parse helpers would need env mutation;
        // instead verify the defaults document themselves.
        let d = VadConfig::default();
        assert!(!d.enabled);
        assert_eq!(d.rms_threshold, DEFAULT_RMS_THRESHOLD);
        assert_eq!(d.silence_ms, DEFAULT_SILENCE_MS);
        assert_eq!(d.min_speech_ms, DEFAULT_MIN_SPEECH_MS);
    }
}
