//! MP3 encoding for the local synthesize path.
//!
//! sherpa-onnx synthesizes 16-bit mono PCM; `format: "mp3"` encodes that PCM
//! via LAME (the `mp3lame-encoder` crate) so the local provider can serve
//! MP3 too. This module owns the PCM→MP3 conversion: encoder config and the
//! encode step. Pure data in/out — no client, no kernel, no model — so it's
//! fully unit-testable without model files.
//!
//! Build note: `mp3lame-encoder` links the C LAME library (`libmp3lame`),
//! which must be present at build time (e.g. `libmp3lame-dev` on Debian).
//! See the plugin README.

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, MonoPcm, Quality};

/// Default MP3 encode bitrate. 128 kbps is the conventional mono-speech
/// default at 24 kHz.
pub const DEFAULT_BITRATE: Bitrate = Bitrate::Kbps128;

/// Encode 16-bit mono PCM into one MP3 buffer (MPEG frames, no ID3 tag).
///
/// `sample_rate` is the PCM rate, passed straight through to LAME (no
/// resampling); it must be a rate LAME accepts (8000/11025/12000/16000/
/// 22050/24000/32000/44100/48000). `channels` must be 1 — sherpa TTS is
/// always mono. Returns the MP3 bytes.
pub fn encode_pcm(pcm: &[i16], sample_rate: u32, channels: u8) -> Result<Vec<u8>, String> {
    if channels != 1 {
        return Err(format!(
            "mp3 encode supports mono only, got {channels} channels"
        ));
    }
    if pcm.is_empty() {
        return Ok(Vec::new());
    }

    let mut encoder = Builder::new()
        .ok_or_else(|| "mp3 encoder init failed".to_string())?
        .with_num_channels(channels)
        .map_err(|e| format!("mp3 channel config failed: {e:?}"))?
        .with_sample_rate(sample_rate)
        .map_err(|e| format!("mp3 sample-rate config failed: {e:?}"))?
        .with_brate(DEFAULT_BITRATE)
        .map_err(|e| format!("mp3 bitrate config failed: {e:?}"))?
        .with_quality(Quality::Best)
        .map_err(|e| format!("mp3 quality config failed: {e:?}"))?
        .build()
        .map_err(|e| format!("mp3 encoder build failed: {e:?}"))?;

    let mut out = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(pcm.len()));
    encoder
        .encode_to_vec(MonoPcm(pcm), &mut out)
        .map_err(|e| format!("mp3 encode failed: {e:?}"))?;
    encoder
        .flush_to_vec::<FlushNoGap>(&mut out)
        .map_err(|e| format!("mp3 flush failed: {e:?}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, seconds: f32) -> Vec<i16> {
        let n = (rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                (t * 440.0 * std::f32::consts::TAU).sin() * 8000.0
            })
            .map(|v| v as i16)
            .collect()
    }

    #[test]
    fn encodes_pcm_to_mpeg_frames() {
        let pcm = sine(24_000, 1.0);
        let mp3 = encode_pcm(&pcm, 24_000, 1).unwrap();
        assert!(!mp3.is_empty(), "non-empty MP3 output");
        // MPEG audio frame sync: 11 set bits -> 0xFF then the top 3 bits of
        // the second byte set (version + sync continuation).
        assert_eq!(mp3[0], 0xFF, "first byte must be the frame sync");
        assert_eq!(mp3[1] & 0xE0, 0xE0, "second byte must carry the sync bits");
    }

    #[test]
    fn empty_pcm_yields_no_bytes() {
        assert!(encode_pcm(&[], 24_000, 1).unwrap().is_empty());
    }

    #[test]
    fn rejects_non_mono() {
        let err = encode_pcm(&[0i16; 100], 24_000, 2).unwrap_err();
        assert!(err.contains("mono only"), "error was: {err}");
    }
}
