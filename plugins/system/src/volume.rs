//! Default-sink volume via host tools: `wpctl` (PipeWire) first, `pactl`
//! (PulseAudio / pipewire-pulse) as fallback. Both are spawned argv-only
//! through [`CommandRunner`]; the text parsers are pure and unit-tested.

use std::sync::Arc;

use async_trait::async_trait;

use crate::backends::{Volume, VolumeStatus};
use crate::error::SystemError;
use crate::request::MuteMode;
use crate::runner::CommandRunner;

/// PipeWire's `wpctl` control of the default sink.
pub struct WpctlVolume {
    runner: Arc<dyn CommandRunner>,
}

impl WpctlVolume {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn run(&self, args: &[&str]) -> Result<String, SystemError> {
        let out = self
            .runner
            .run("wpctl", args)
            .await
            .map_err(|e| SystemError::Backend(format!("wpctl failed: {e}")))?;
        if !out.ok {
            return Err(SystemError::Backend(format!(
                "wpctl exited nonzero: {}",
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }
}

#[async_trait]
impl Volume for WpctlVolume {
    async fn get(&self) -> Result<VolumeStatus, SystemError> {
        let stdout = self.run(&["get-volume", "@DEFAULT_AUDIO_SINK@"]).await?;
        parse_wpctl(&stdout)
            .ok_or_else(|| SystemError::Backend(format!("unparseable wpctl output: {stdout:?}")))
    }

    async fn set(&self, percent: u8) -> Result<VolumeStatus, SystemError> {
        let frac = format!("{:.2}", f64::from(percent) / 100.0);
        self.run(&["set-volume", "@DEFAULT_AUDIO_SINK@", &frac])
            .await?;
        self.get().await
    }

    async fn mute(&self, mode: MuteMode) -> Result<VolumeStatus, SystemError> {
        self.run(&["set-mute", "@DEFAULT_AUDIO_SINK@", mode.as_tool_arg()])
            .await?;
        self.get().await
    }
}

/// PulseAudio's `pactl` against the default sink (also works under
/// pipewire-pulse).
pub struct PactlVolume {
    runner: Arc<dyn CommandRunner>,
}

impl PactlVolume {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn run(&self, args: &[&str]) -> Result<String, SystemError> {
        let out = self
            .runner
            .run("pactl", args)
            .await
            .map_err(|e| SystemError::Backend(format!("pactl failed: {e}")))?;
        if !out.ok {
            return Err(SystemError::Backend(format!(
                "pactl exited nonzero: {}",
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }
}

#[async_trait]
impl Volume for PactlVolume {
    async fn get(&self) -> Result<VolumeStatus, SystemError> {
        let vol_out = self.run(&["get-sink-volume", "@DEFAULT_SINK@"]).await?;
        let mute_out = self.run(&["get-sink-mute", "@DEFAULT_SINK@"]).await?;
        let percent = parse_pactl_volume(&vol_out).ok_or_else(|| {
            SystemError::Backend(format!("unparseable pactl volume output: {vol_out:?}"))
        })?;
        let muted = parse_pactl_mute(&mute_out).ok_or_else(|| {
            SystemError::Backend(format!("unparseable pactl mute output: {mute_out:?}"))
        })?;
        Ok(VolumeStatus { percent, muted })
    }

    async fn set(&self, percent: u8) -> Result<VolumeStatus, SystemError> {
        let arg = format!("{percent}%");
        self.run(&["set-sink-volume", "@DEFAULT_SINK@", &arg])
            .await?;
        self.get().await
    }

    async fn mute(&self, mode: MuteMode) -> Result<VolumeStatus, SystemError> {
        self.run(&["set-sink-mute", "@DEFAULT_SINK@", mode.as_tool_arg()])
            .await?;
        self.get().await
    }
}

/// Parse `wpctl get-volume` output: `Volume: 0.65` with an optional
/// ` [MUTED]` suffix.
pub fn parse_wpctl(stdout: &str) -> Option<VolumeStatus> {
    let rest = stdout.trim().strip_prefix("Volume:")?.trim();
    let (frac_part, muted) = match rest.strip_suffix("[MUTED]") {
        Some(head) => (head.trim(), true),
        None => (rest, false),
    };
    let frac: f64 = frac_part.trim().parse().ok()?;
    if !(0.0..=1.0).contains(&frac) {
        return None;
    }
    Some(VolumeStatus {
        percent: (frac * 100.0).round() as u32,
        muted,
    })
}

/// Parse the first `/ NN%` field from `pactl get-sink-volume` output:
/// `Volume: front-left: 45875 /  70% / -4.50 dB, ...`
pub fn parse_pactl_volume(stdout: &str) -> Option<u32> {
    let idx = stdout.find('/')?;
    let tail = stdout[idx..].trim_start_matches('/');
    let percent_txt = tail.trim().split('%').next()?.trim();
    let percent: u32 = percent_txt.parse().ok()?;
    (percent <= 100).then_some(percent)
}

/// Parse `pactl get-sink-mute` output: `Mute: yes|no`.
pub fn parse_pactl_mute(stdout: &str) -> Option<bool> {
    match stdout.trim().strip_prefix("Mute:")?.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpctl_parses_plain_fraction() {
        assert_eq!(
            parse_wpctl("Volume: 0.65\n"),
            Some(VolumeStatus {
                percent: 65,
                muted: false
            })
        );
        assert_eq!(
            parse_wpctl("Volume: 1.00"),
            Some(VolumeStatus {
                percent: 100,
                muted: false
            })
        );
        assert_eq!(
            parse_wpctl("Volume: 0.0"),
            Some(VolumeStatus {
                percent: 0,
                muted: false
            })
        );
    }

    #[test]
    fn wpctl_parses_muted_suffix() {
        assert_eq!(
            parse_wpctl("Volume: 0.42 [MUTED]\n"),
            Some(VolumeStatus {
                percent: 42,
                muted: true
            })
        );
    }

    #[test]
    fn wpctl_rejects_garbage_and_out_of_range() {
        assert_eq!(parse_wpctl(""), None);
        assert_eq!(parse_wpctl("Volume: banana"), None);
        assert_eq!(parse_wpctl("Volume: 1.5"), None);
        assert_eq!(parse_wpctl("no prefix here"), None);
    }

    #[test]
    fn wpctl_rounds_fractions_to_percent() {
        assert_eq!(parse_wpctl("Volume: 0.655").map(|v| v.percent), Some(66));
    }

    #[test]
    fn pactl_parses_first_channel_percentage() {
        let out =
            "Volume: front-left: 45875 /  70% / -4.50 dB,   front-right: 45875 /  70% / -4.50 dB\n";
        assert_eq!(parse_pactl_volume(out), Some(70));
    }

    #[test]
    fn pactl_rejects_garbage_and_over_100() {
        assert_eq!(parse_pactl_volume("no slashes"), None);
        assert_eq!(parse_pactl_volume("/ 130%"), None);
        assert_eq!(parse_pactl_volume("/ abc%"), None);
    }

    #[test]
    fn pactl_parses_mute_states() {
        assert_eq!(parse_pactl_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_pactl_mute("Mute: no\n"), Some(false));
        assert_eq!(parse_pactl_mute("Mute: maybe"), None);
        assert_eq!(parse_pactl_mute(""), None);
    }
}
