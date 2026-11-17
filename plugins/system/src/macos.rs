//! macOS spawn-wiring over the shared [`CommandRunner`] seam: `pmset` for
//! battery, `osascript` for volume, CGSession for lock. All argv-only.
//! The output parsers live in the non-gated `macos_parse` so Linux CI
//! tests them; only these thin impls are macOS-only.

use std::sync::Arc;

use async_trait::async_trait;

use crate::backends::{Battery, SessionLock, Volume, VolumeStatus};
use crate::error::SystemError;
use crate::macos_parse::{parse_pmset_batt, parse_volume_settings};
use crate::request::MuteMode;
use crate::runner::CommandRunner;

const CGSESSION: &str =
    "/System/Library/CoreServices/Menu Extras/User.menu/Contents/Resources/CGSession";

pub struct MacosBattery {
    runner: Arc<dyn CommandRunner>,
}

impl MacosBattery {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl Battery for MacosBattery {
    async fn status(&self) -> Result<crate::backends::BatteryStatus, SystemError> {
        let out = self
            .runner
            .run("pmset", &["-g", "batt"])
            .await
            .map_err(|e| SystemError::Backend(format!("pmset failed: {e}")))?;
        if !out.ok {
            return Err(SystemError::Backend(format!(
                "pmset exited nonzero: {}",
                out.stderr.trim()
            )));
        }
        parse_pmset_batt(&out.stdout).ok_or_else(|| {
            SystemError::Backend(format!("no battery in pmset output: {:?}", out.stdout))
        })
    }
}

pub struct MacosVolume {
    runner: Arc<dyn CommandRunner>,
}

impl MacosVolume {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn osascript(&self, script: &str) -> Result<String, SystemError> {
        let out = self
            .runner
            .run("osascript", &["-e", script])
            .await
            .map_err(|e| SystemError::Backend(format!("osascript failed: {e}")))?;
        if !out.ok {
            return Err(SystemError::Backend(format!(
                "osascript exited nonzero: {}",
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }
}

#[async_trait]
impl Volume for MacosVolume {
    async fn get(&self) -> Result<VolumeStatus, SystemError> {
        let stdout = self.osascript("get volume settings").await?;
        parse_volume_settings(&stdout)
            .ok_or_else(|| SystemError::Backend(format!("unparseable volume settings: {stdout:?}")))
    }

    async fn set(&self, percent: u8) -> Result<VolumeStatus, SystemError> {
        self.osascript(&format!("set volume output volume {percent}"))
            .await?;
        self.get().await
    }

    async fn mute(&self, mode: MuteMode) -> Result<VolumeStatus, SystemError> {
        let muted = match mode {
            MuteMode::On => true,
            MuteMode::Off => false,
            // AppleScript has no toggle — read, then write the inverse.
            MuteMode::Toggle => !self.get().await?.muted,
        };
        self.osascript(&format!("set volume output muted {muted}"))
            .await?;
        self.get().await
    }
}

pub struct MacosLock {
    runner: Arc<dyn CommandRunner>,
}

impl MacosLock {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl SessionLock for MacosLock {
    async fn lock(&self) -> Result<(), SystemError> {
        let out = self
            .runner
            .run(CGSESSION, &["-suspend"])
            .await
            .map_err(|e| SystemError::Backend(format!("CGSession failed: {e}")))?;
        if !out.ok {
            return Err(SystemError::Backend(format!(
                "CGSession exited nonzero: {}",
                out.stderr.trim()
            )));
        }
        Ok(())
    }
}
