//! Battery via UPower on the system D-Bus (Linux).
//!
//! Reads the aggregated `DisplayDevice` object — the same source GNOME/KDE
//! battery applets use — so laptops with multiple batteries/UPSes report
//! one coherent number. Property reads go through zbus's cached proxy;
//! the UPower `State`/time mapping is pure and unit-tested.

use async_trait::async_trait;
use zbus::Connection;

use crate::backends::{Battery, BatteryState, BatteryStatus};
use crate::error::SystemError;

/// zbus proxy over `org.freedesktop.UPower.Device` pinned to the
/// aggregated DisplayDevice path.
#[zbus::proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/DisplayDevice"
)]
trait UpowerDevice {
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<f64>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    /// Seconds to empty; `-1` when unknown.
    #[zbus(property)]
    fn time_to_empty(&self) -> zbus::Result<i64>;

    /// Seconds to full; `-1` when unknown.
    #[zbus(property)]
    fn time_to_full(&self) -> zbus::Result<i64>;
}

pub struct UpowerBattery {
    device: UpowerDeviceProxy<'static>,
}

impl UpowerBattery {
    /// Connect and verify the DisplayDevice answers, so detection can
    /// distinguish "no battery" from "battery present".
    pub async fn connect(conn: Connection) -> Result<Self, SystemError> {
        let device = UpowerDeviceProxy::new(&conn)
            .await
            .map_err(|e| SystemError::Backend(format!("UPower DisplayDevice unavailable: {e}")))?;
        // Probe one property: a desktop without a battery still exposes the
        // object but returns 0.0 percent / Unknown state — that's fine, it
        // IS the reading then. Only transport failure means no backend.
        device
            .percentage()
            .await
            .map_err(|e| SystemError::Backend(format!("UPower probe failed: {e}")))?;
        Ok(Self { device })
    }
}

#[async_trait]
impl Battery for UpowerBattery {
    async fn status(&self) -> Result<BatteryStatus, SystemError> {
        let percent = self
            .device
            .percentage()
            .await
            .map_err(|e| SystemError::Backend(format!("UPower percentage: {e}")))?;
        let state = self
            .device
            .state()
            .await
            .map_err(|e| SystemError::Backend(format!("UPower state: {e}")))?;
        let tte = self
            .device
            .time_to_empty()
            .await
            .map_err(|e| SystemError::Backend(format!("UPower time-to-empty: {e}")))?;
        let ttf = self
            .device
            .time_to_full()
            .await
            .map_err(|e| SystemError::Backend(format!("UPower time-to-full: {e}")))?;

        Ok(BatteryStatus {
            percent,
            state: map_state(state),
            time_to_empty_s: map_seconds(tte),
            time_to_full_s: map_seconds(ttf),
        })
    }
}

/// Map UPower's `State` enum onto our normalized states.
///
/// UPower values: 0 unknown, 1 charging, 2 discharging, 3 empty,
/// 4 fully-charged, 5 pending-charge, 6 pending-discharge. Pending states
/// fold into their direction (the battery is effectively sitting at the
/// charge end of that transition).
pub const fn map_state(raw: u32) -> BatteryState {
    match raw {
        1 | 5 => BatteryState::Charging,
        2 | 6 => BatteryState::Discharging,
        3 => BatteryState::Empty,
        4 => BatteryState::Full,
        _ => BatteryState::Unknown,
    }
}

/// UPower uses `-1` for "unknown" durations.
pub const fn map_seconds(raw: i64) -> Option<u64> {
    if raw < 0 {
        None
    } else {
        Some(raw as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_mapping_covers_all_upower_values() {
        assert_eq!(map_state(0), BatteryState::Unknown);
        assert_eq!(map_state(1), BatteryState::Charging);
        assert_eq!(map_state(2), BatteryState::Discharging);
        assert_eq!(map_state(3), BatteryState::Empty);
        assert_eq!(map_state(4), BatteryState::Full);
        assert_eq!(map_state(5), BatteryState::Charging);
        assert_eq!(map_state(6), BatteryState::Discharging);
        // Future/reserved values degrade to Unknown, never panic.
        assert_eq!(map_state(7), BatteryState::Unknown);
        assert_eq!(map_state(u32::MAX), BatteryState::Unknown);
    }

    #[test]
    fn seconds_minus_one_is_unknown() {
        assert_eq!(map_seconds(-1), None);
        assert_eq!(map_seconds(0), Some(0));
        assert_eq!(map_seconds(3600), Some(3600));
    }

    #[test]
    fn state_strings_are_wire_stable() {
        assert_eq!(BatteryState::Charging.as_str(), "charging");
        assert_eq!(BatteryState::Discharging.as_str(), "discharging");
        assert_eq!(BatteryState::Full.as_str(), "full");
        assert_eq!(BatteryState::Empty.as_str(), "empty");
        assert_eq!(BatteryState::Unknown.as_str(), "unknown");
    }
}
