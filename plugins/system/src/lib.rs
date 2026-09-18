//! `system` plugin — local host queries and simple, reversible controls.
//!
//! One domain: the state of the machine this plugin runs on. Read-only
//! actions (`sys_info`, `sys_battery`, `sys_procs`, `sys_volume`,
//! `sys_brightness`, `sys_power_profile`) plus reversible setters
//! (`sys_volume_set`, `sys_volume_mute`, `sys_brightness_set`,
//! `sys_lock`, `sys_power_profile_set`). Deliberately NOT here:
//! destructive or differently-scoped actions (kill process, network
//! config) — those would turn this into shell-lite (see root
//! `ROADMAP.md`).

pub mod backends;
pub mod brightness;
pub mod detect;
pub mod error;
pub mod info;
#[cfg(target_os = "linux")]
pub mod lock;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod macos_parse;
pub mod power_profile;
pub mod request;
pub mod runner;
#[cfg(target_os = "linux")]
pub mod upower;
pub mod volume;

use std::sync::Arc;

use serde_json::json;
use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus, Envelope, PluginManifest};
use vynkor_sdk::{Plugin, VynkorError};

use crate::backends::SystemBackends;
use crate::error::SystemError;
use crate::request::SysRequest;

pub const PLUGIN_ID: &str = "system";
pub const PLUGIN_VERSION: &str = "0.3.0";

/// Actions this plugin serves; must stay in sync with `plugin.json` and the
/// kernel refuses ambiguous manifest declarations anyway.
pub const ACTIONS: &[&str] = &[
    "sys_info",
    "sys_battery",
    "sys_procs",
    "sys_volume",
    "sys_volume_set",
    "sys_volume_mute",
    "sys_brightness",
    "sys_brightness_set",
    "sys_lock",
    "sys_power_profile",
    "sys_power_profile_set",
];

/// Dispatch one action to its backend.
///
/// Raw params cross into typed values exactly once (`request::parse`);
/// every rejection names the offending field.
pub async fn handle_action(
    action: &str,
    params_json: &[u8],
    be: &SystemBackends,
) -> Result<serde_json::Value, SystemError> {
    match request::parse(action, params_json)? {
        SysRequest::NoParams => handle_get(action, be).await,
        SysRequest::VolumeSet { percent } => {
            let v = require(be.volume.as_ref(), "volume")?.set(percent).await?;
            encode(volume_json(v))
        }
        SysRequest::VolumeMute { mode } => {
            let v = require(be.volume.as_ref(), "volume")?.mute(mode).await?;
            encode(volume_json(v))
        }
        SysRequest::BrightnessSet { percent } => {
            let percent = require(be.brightness.as_ref(), "brightness")?.set(percent).await?;
            encode(json!({ "percent": percent }))
        }
        SysRequest::PowerProfileSet { profile } => {
            let state = require(be.power.as_ref(), "power-profiles-daemon")?.set(profile).await?;
            encode(serde_json::to_value(state).map_err(encode_err)?)
        }
    }
}

async fn handle_get(action: &str, be: &SystemBackends) -> Result<serde_json::Value, SystemError> {
    match action {
        "sys_info" => encode(info::sys_info()),
        "sys_procs" => encode(info::sys_procs()),
        "sys_battery" => {
            let s = require(be.battery.as_ref(), "battery")?.status().await?;
            encode(json!({
                "percent": s.percent,
                "state": s.state.as_str(),
                "time_to_empty_s": s.time_to_empty_s,
                "time_to_full_s": s.time_to_full_s,
            }))
        }
        "sys_volume" => {
            let s = require(be.volume.as_ref(), "volume")?.get().await?;
            encode(volume_json(s))
        }
        "sys_brightness" => {
            let percent = require(be.brightness.as_ref(), "brightness")?.get().await?;
            encode(json!({ "percent": percent }))
        }
        "sys_power_profile" => {
            let s = require(be.power.as_ref(), "power-profiles-daemon")?.get().await?;
            encode(serde_json::to_value(s).map_err(encode_err)?)
        }
        "sys_lock" => {
            require(be.lock.as_ref(), "session-lock")?.lock().await?;
            encode(json!({ "ok": true }))
        }
        // request::parse already validated membership against ACTIONS.
        other => Err(SystemError::UnknownAction(other.to_string())),
    }
}

fn volume_json(s: backends::VolumeStatus) -> serde_json::Value {
    json!({ "percent": s.percent, "muted": s.muted })
}

fn require<'a, T>(slot: Option<&'a T>, capability: &'static str) -> Result<&'a T, SystemError> {
    slot.ok_or(SystemError::NotSupported(capability))
}

fn encode(v: impl serde::Serialize) -> Result<serde_json::Value, SystemError> {
    serde_json::to_value(v).map_err(encode_err)
}

fn encode_err(e: serde_json::Error) -> SystemError {
    SystemError::Backend(format!("encode response: {e}"))
}

/// The SDK-facing plugin: stock serve loop, dispatch above.
/// Lives in the lib so integration tests can drive it over a socket pair.
pub struct SystemPlugin {
    pub backends: Arc<SystemBackends>,
}

impl SystemPlugin {
    pub fn new(backends: Arc<SystemBackends>) -> Self {
        Self { backends }
    }
}

impl Plugin for SystemPlugin {
    fn id(&self) -> &str {
        PLUGIN_ID
    }

    fn version(&self) -> &str {
        PLUGIN_VERSION
    }

    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            // Broad access by design — keep strict (root ROADMAP): this is
            // the one permission covering local system state queries and
            // reversible setters.
            permissions: vec!["PERMISSION_SYSTEM".into()],
            actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
            action_specs: vynkor_plugin_manifest::action_specs(),
            ..Default::default()
        }
    }

    async fn on_message(&mut self, envelope: Envelope) -> Result<Option<Envelope>, VynkorError> {
        let Some(envelope::Payload::ActionRequest(req)) = envelope.payload else {
            return Ok(None);
        };
        let reply = match handle_action(&req.action, &req.params_json, &self.backends).await {
            Ok(value) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionOk as i32,
                data_json: value.to_string().into_bytes(),
                error: String::new(),
            },
            Err(e) => {
                let not_found = e.code().as_str() == "ERR_SYS_NOT_FOUND";
                ActionResponse {
                    action_id: req.action_id,
                    status: if not_found {
                        ActionStatus::ActionNotFound as i32
                    } else {
                        ActionStatus::ActionError as i32
                    },
                    data_json: Vec::new(),
                    error: e.to_string(),
                }
            }
        };
        Ok(Some(Envelope {
            payload: Some(envelope::Payload::ActionResponse(reply)),
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{Battery, BatteryState, BatteryStatus, Volume, VolumeStatus};
    use std::sync::Arc;

    struct FakeBattery;
    #[async_trait::async_trait]
    impl Battery for FakeBattery {
        async fn status(&self) -> Result<BatteryStatus, SystemError> {
            Ok(BatteryStatus {
                percent: 87.5,
                state: BatteryState::Discharging,
                time_to_empty_s: Some(4210),
                time_to_full_s: None,
            })
        }
    }

    struct FakeVolume;
    #[async_trait::async_trait]
    impl Volume for FakeVolume {
        async fn get(&self) -> Result<VolumeStatus, SystemError> {
            Ok(VolumeStatus { percent: 42, muted: false })
        }

        async fn set(&self, percent: u8) -> Result<VolumeStatus, SystemError> {
            Ok(VolumeStatus { percent: u32::from(percent), muted: false })
        }

        async fn mute(&self, mode: crate::request::MuteMode) -> Result<VolumeStatus, SystemError> {
            Ok(VolumeStatus { percent: 42, muted: mode == crate::request::MuteMode::On })
        }
    }

    fn full_backends() -> SystemBackends {
        SystemBackends {
            battery: Some(Arc::new(FakeBattery)),
            volume: Some(Arc::new(FakeVolume)),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn sys_battery_maps_backend_status() {
        let v = handle_action("sys_battery", b"{}", &full_backends()).await.expect("ok");
        assert_eq!(v["percent"], 87.5);
        assert_eq!(v["state"], "discharging");
        assert_eq!(v["time_to_empty_s"], 4210);
        assert_eq!(v["time_to_full_s"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn missing_backend_is_not_supported_naming_the_capability() {
        let be = SystemBackends::default();
        for (action, capability) in [
            ("sys_battery", "battery"),
            ("sys_volume", "volume"),
            ("sys_brightness", "brightness"),
            ("sys_lock", "session-lock"),
            ("sys_power_profile", "power-profiles-daemon"),
        ] {
            let e = handle_action(action, b"", &be).await.unwrap_err();
            assert_eq!(e.code().as_str(), "ERR_SYS_NOT_SUPPORTED", "{action}");
            assert!(e.to_string().contains(capability), "{action}");
        }
    }

    #[tokio::test]
    async fn setters_return_resulting_reading() {
        let v = handle_action("sys_volume_set", br#"{"percent":77}"#, &full_backends())
            .await
            .expect("ok");
        assert_eq!(v["percent"], 77);
        assert_eq!(v["muted"], false);

        let v = handle_action("sys_volume_mute", br#"{"mode":"on"}"#, &full_backends())
            .await
            .expect("ok");
        assert_eq!(v["muted"], true);
    }

    #[tokio::test]
    async fn unknown_action_is_not_found() {
        let e =
            handle_action("sys_frobnicate", b"{}", &full_backends()).await.unwrap_err();
        assert_eq!(e.code().as_str(), "ERR_SYS_NOT_FOUND");
    }

    #[tokio::test]
    async fn malformed_params_are_bad_params_not_a_crash() {
        let e = handle_action("sys_info", b"{broken", &full_backends()).await.unwrap_err();
        assert_eq!(e.code().as_str(), "ERR_SYS_BAD_PARAMS");
        let e = handle_action("sys_volume_set", b"[1]", &full_backends()).await.unwrap_err();
        assert_eq!(e.code().as_str(), "ERR_SYS_BAD_PARAMS");
    }
}

#[cfg(test)]
mod manifest_specs_tests {
    use serde_json::Value;

    // Regression guard: an action that reaches the model with an empty
    // description gets dropped by the agent's embedding filter. This reads
    // the shipped plugin.json directly (not action_specs(), which relies on
    // current_exe() and is meaningless in a dev-build test binary).
    #[test]
    fn shipped_manifest_documents_every_declared_action() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let declared = parsed["actions"].as_array().unwrap().len();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        assert_eq!(specs.len(), declared, "every declared action must produce a spec");
        let undocumented: Vec<&str> = specs
            .iter()
            .filter(|s| s.description.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(undocumented.is_empty(), "actions missing a description: {undocumented:?}");
    }
}
