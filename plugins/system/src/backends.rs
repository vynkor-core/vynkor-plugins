//! Domain traits and shared value types for the `system` backends.
//!
//! Each capability is a small trait so backends compose per-host: detection
//! (`detect::detect`) fills [`SystemBackends`] with whatever the host
//! actually provides, and every `Option` that ends up `None` surfaces to
//! callers as `ERR_SYS_NOT_SUPPORTED` naming the capability — the same
//! graceful-degradation shape as `media`'s capability guards.

use std::sync::Arc;

use crate::error::SystemError;

/// Battery charge state, normalized across UPower (Linux) and pmset (macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatteryState {
    #[default]
    Unknown,
    Charging,
    Discharging,
    Empty,
    Full,
}

impl BatteryState {
    /// Wire representation used in `sys_battery` responses.
    pub const fn as_str(self) -> &'static str {
        match self {
            BatteryState::Unknown => "unknown",
            BatteryState::Charging => "charging",
            BatteryState::Discharging => "discharging",
            BatteryState::Empty => "empty",
            BatteryState::Full => "full",
        }
    }
}

/// One battery reading.
#[derive(Debug, Clone, PartialEq)]
pub struct BatteryStatus {
    pub percent: f64,
    pub state: BatteryState,
    /// Seconds to empty, when the platform reports it (`None` = unknown,
    /// e.g. while charging or on fresh UPower data).
    pub time_to_empty_s: Option<u64>,
    /// Seconds to full, same contract as [`BatteryStatus::time_to_empty_s`].
    pub time_to_full_s: Option<u64>,
}

/// Default-output volume reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeStatus {
    /// 0..=100, rounded from the tool's fraction.
    pub percent: u32,
    pub muted: bool,
}

#[async_trait::async_trait]
pub trait Battery: Send + Sync {
    async fn status(&self) -> Result<BatteryStatus, SystemError>;
}

#[async_trait::async_trait]
pub trait Volume: Send + Sync {
    async fn get(&self) -> Result<VolumeStatus, SystemError>;
    /// Set absolute volume 0..=100; returns the resulting reading.
    async fn set(&self, percent: u8) -> Result<VolumeStatus, SystemError>;
    /// Apply a mute mode; returns the resulting reading.
    async fn mute(&self, mode: crate::request::MuteMode) -> Result<VolumeStatus, SystemError>;
}

#[async_trait::async_trait]
pub trait Brightness: Send + Sync {
    async fn get(&self) -> Result<u8, SystemError>;
    /// Set absolute brightness 0..=100 (0 clamps to the non-blanking
    /// floor); returns the resulting reading.
    async fn set(&self, percent: u8) -> Result<u8, SystemError>;
}

#[async_trait::async_trait]
pub trait SessionLock: Send + Sync {
    async fn lock(&self) -> Result<(), SystemError>;
}

#[async_trait::async_trait]
pub trait PowerProfiles: Send + Sync {
    async fn get(&self) -> Result<crate::power_profile::ProfileState, SystemError>;
    async fn set(
        &self,
        profile: crate::request::Profile,
    ) -> Result<crate::power_profile::ProfileState, SystemError>;
}

/// The set of backends detected on this host. `None` = capability absent →
/// `ERR_SYS_NOT_SUPPORTED`.
#[derive(Clone, Default)]
pub struct SystemBackends {
    pub battery: Option<Arc<dyn Battery>>,
    pub volume: Option<Arc<dyn Volume>>,
    pub brightness: Option<Arc<dyn Brightness>>,
    pub lock: Option<Arc<dyn SessionLock>>,
    pub power: Option<Arc<dyn PowerProfiles>>,
}
