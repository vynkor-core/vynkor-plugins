//! Typed request parsing at the action boundary.
//!
//! Parse-don't-validate: raw `params_json` crosses into typed values
//! exactly once, here; dispatch code downstream only ever sees valid
//! typed requests. Every rejection names the offending field — serde
//! enforces nothing beyond types, and a manifest `"minimum"` is
//! documentation, not a check.

use crate::error::SystemError;

/// Mute target state: explicit, so callers never guess current state to
/// mute/unmute (`toggle` exists for the rare "just flip it" case).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuteMode {
    On,
    Off,
    Toggle,
}

impl MuteMode {
    pub const fn as_tool_arg(self) -> &'static str {
        match self {
            MuteMode::On => "1",
            MuteMode::Off => "0",
            MuteMode::Toggle => "toggle",
        }
    }
}

/// Power profile, mirroring power-profiles-daemon's wire strings exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Performance,
    Balanced,
    PowerSaver,
}

impl Profile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Profile::Performance => "performance",
            Profile::Balanced => "balanced",
            Profile::PowerSaver => "power-saver",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "performance" => Some(Profile::Performance),
            "balanced" => Some(Profile::Balanced),
            "power-saver" | "power_saver" | "saver" => Some(Profile::PowerSaver),
            _ => None,
        }
    }
}

/// One parsed setter/getter request. Getters carry no payload.
#[derive(Debug, Clone, PartialEq)]
pub enum SysRequest {
    NoParams,
    VolumeSet { percent: u8 },
    VolumeMute { mode: MuteMode },
    BrightnessSet { percent: u8 },
    PowerProfileSet { profile: Profile },
}

/// Percent-typed newtype range check shared by volume and brightness
/// setters: integer 0..=100 only.
fn parse_percent(v: &serde_json::Value, field: &'static str) -> Result<u8, SystemError> {
    let n = v
        .as_u64()
        .ok_or_else(|| SystemError::BadParams(format!("'{field}' must be an integer 0..=100")))?;
    u8::try_from(n)
        .ok()
        .filter(|p| *p <= 100)
        .ok_or_else(|| SystemError::BadParams(format!("'{field}' must be 0..=100")))
}

fn field<'a>(v: &'a serde_json::Value, name: &str) -> Result<&'a serde_json::Value, SystemError> {
    v.get(name)
        .ok_or_else(|| SystemError::BadParams(format!("missing '{name}'")))
}

fn expect_object(params_json: &[u8]) -> Result<serde_json::Value, SystemError> {
    let trimmed = trim_ascii(params_json);
    let v: serde_json::Value = serde_json::from_slice(trimmed)
        .map_err(|e| SystemError::BadParams(format!("params must be a JSON object: {e}")))?;
    match v {
        obj @ serde_json::Value::Object(_) => Ok(obj),
        _ => Err(SystemError::BadParams(
            "params must be a JSON object".to_string(),
        )),
    }
}

/// Parse one request. Unknown actions are the dispatcher's business —
/// this function assumes the name is known.
pub fn parse(action: &str, params_json: &[u8]) -> Result<SysRequest, SystemError> {
    match action {
        "sys_info" | "sys_battery" | "sys_procs" | "sys_volume" | "sys_brightness"
        | "sys_power_profile" | "sys_lock" => no_params(params_json),
        "sys_volume_set" => Ok(SysRequest::VolumeSet {
            percent: parse_percent(field(&expect_object(params_json)?, "percent")?, "percent")?,
        }),
        "sys_brightness_set" => Ok(SysRequest::BrightnessSet {
            percent: parse_percent(field(&expect_object(params_json)?, "percent")?, "percent")?,
        }),
        "sys_volume_mute" => {
            let obj = expect_object(params_json)?;
            let mode = field(&obj, "mode")?;
            let mode = mode.as_str().ok_or_else(|| {
                SystemError::BadParams("'mode' must be on|off|toggle".to_string())
            })?;
            let mode = match mode {
                "on" => MuteMode::On,
                "off" => MuteMode::Off,
                "toggle" => MuteMode::Toggle,
                other => {
                    return Err(SystemError::BadParams(format!(
                        "'mode' must be on|off|toggle, got '{other}'"
                    )))
                }
            };
            Ok(SysRequest::VolumeMute { mode })
        }
        "sys_power_profile_set" => {
            let obj = expect_object(params_json)?;
            let p = field(&obj, "profile")?;
            let p = p.as_str().ok_or_else(|| {
                SystemError::BadParams(
                    "'profile' must be performance|balanced|power-saver".to_string(),
                )
            })?;
            let profile = Profile::parse(p).ok_or_else(|| {
                SystemError::BadParams(format!(
                    "'profile' must be performance|balanced|power-saver, got '{p}'"
                ))
            })?;
            Ok(SysRequest::PowerProfileSet { profile })
        }
        // Dispatcher checks membership before calling; this arm is
        // defense-in-depth, never a panic path.
        other => Err(SystemError::UnknownAction(other.to_string())),
    }
}

fn no_params(params_json: &[u8]) -> Result<SysRequest, SystemError> {
    let trimmed = trim_ascii(params_json);
    if trimmed.is_empty() {
        return Ok(SysRequest::NoParams);
    }
    match expect_object(trimmed)? {
        serde_json::Value::Object(ref map) if map.is_empty() => Ok(SysRequest::NoParams),
        _ => Err(SystemError::BadParams(
            "this action takes no parameters".to_string(),
        )),
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let not_ws = |b: &u8| !b.is_ascii_whitespace();
    let Some(start) = bytes.iter().position(not_ws) else {
        return &[];
    };
    let end = bytes
        .iter()
        .rposition(not_ws)
        .expect("start implies a non-ws byte exists");
    &bytes[start..=end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getters_accept_empty_and_reject_extra_params() {
        for action in [
            "sys_info",
            "sys_battery",
            "sys_procs",
            "sys_volume",
            "sys_lock",
        ] {
            assert_eq!(parse(action, b"").unwrap(), SysRequest::NoParams);
            assert_eq!(parse(action, b"  {} \n").unwrap(), SysRequest::NoParams);
            assert_eq!(
                parse(action, br#"{"x":1}"#).unwrap_err().code().as_str(),
                "ERR_SYS_BAD_PARAMS"
            );
        }
    }

    #[test]
    fn volume_set_requires_integer_percent_in_range() {
        assert_eq!(
            parse("sys_volume_set", br#"{"percent": 42}"#).unwrap(),
            SysRequest::VolumeSet { percent: 42 }
        );
        assert_eq!(
            parse("sys_volume_set", br#"{"percent": 0}"#)
                .unwrap()
                .clone(),
            SysRequest::VolumeSet { percent: 0 }
        );
        assert_eq!(
            parse("sys_volume_set", br#"{"percent": 101}"#)
                .unwrap_err()
                .code()
                .as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
        assert_eq!(
            parse("sys_volume_set", br#"{"percent": 42.5}"#)
                .unwrap_err()
                .code()
                .as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
        assert_eq!(
            parse("sys_volume_set", br#"{"percent": "42"}"#)
                .unwrap_err()
                .code()
                .as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
        assert_eq!(
            parse("sys_volume_set", br#"{}"#).unwrap_err().to_string(),
            "ERR_SYS_BAD_PARAMS: missing 'percent'"
        );
    }

    #[test]
    fn volume_mute_parses_explicit_modes_only() {
        assert_eq!(
            parse("sys_volume_mute", br#"{"mode":"on"}"#).unwrap(),
            SysRequest::VolumeMute { mode: MuteMode::On }
        );
        assert_eq!(
            parse("sys_volume_mute", br#"{"mode":"toggle"}"#).unwrap(),
            SysRequest::VolumeMute {
                mode: MuteMode::Toggle
            }
        );
        assert_eq!(
            parse("sys_volume_mute", br#"{"mode":"banana"}"#)
                .unwrap_err()
                .code()
                .as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
        assert_eq!(
            parse("sys_volume_mute", br#"{"mode":true}"#)
                .unwrap_err()
                .code()
                .as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
    }

    #[test]
    fn brightness_set_shares_percent_validation() {
        assert_eq!(
            parse("sys_brightness_set", br#"{"percent":100}"#).unwrap(),
            SysRequest::BrightnessSet { percent: 100 }
        );
        assert_eq!(
            parse("sys_brightness_set", br#"{"percent":-1}"#)
                .unwrap_err()
                .code()
                .as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
    }

    #[test]
    fn power_profile_set_parses_wire_strings_with_aliases() {
        assert_eq!(
            parse("sys_power_profile_set", br#"{"profile":"power-saver"}"#).unwrap(),
            SysRequest::PowerProfileSet {
                profile: Profile::PowerSaver
            }
        );
        assert_eq!(
            parse("sys_power_profile_set", br#"{"profile":"saver"}"#).unwrap(),
            SysRequest::PowerProfileSet {
                profile: Profile::PowerSaver
            }
        );
        assert_eq!(
            parse("sys_power_profile_set", br#"{"profile":"turbo"}"#)
                .unwrap_err()
                .code()
                .as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
    }

    #[test]
    fn mute_mode_maps_to_tool_args() {
        assert_eq!(MuteMode::On.as_tool_arg(), "1");
        assert_eq!(MuteMode::Off.as_tool_arg(), "0");
        assert_eq!(MuteMode::Toggle.as_tool_arg(), "toggle");
    }

    #[test]
    fn profile_wire_strings_roundtrip() {
        for s in ["performance", "balanced", "power-saver"] {
            assert_eq!(Profile::parse(s).map(Profile::as_str), Some(s));
        }
        assert_eq!(Profile::parse("PowerSaver"), None);
    }

    #[test]
    fn malformed_json_is_bad_params_not_a_crash() {
        assert_eq!(
            parse("sys_info", b"{broken").unwrap_err().code().as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
        assert_eq!(
            parse("sys_volume_set", b"[1]").unwrap_err().code().as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
    }
}
