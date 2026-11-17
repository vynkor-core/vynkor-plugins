//! Pure output parsers for macOS host tools (`pmset`, `osascript`).
//!
//! Deliberately NOT cfg-gated: string parsing is where the bugs live, and
//! keeping it compiled on every platform means Linux CI exercises it
//! against fixture outputs. Only the spawn wiring lives behind the macOS
//! gate (`macos.rs`).

use crate::backends::{BatteryState, BatteryStatus, VolumeStatus};

/// Parse `pmset -g batt` output:
///
/// ```text
/// Now drawing from 'Battery Power'
///  -InternalBattery-0 (id=12345678)    87%; discharging; 4:23 remaining present: true
/// ```
///
/// State words observed in the wild: `discharging`, `charging`,
/// `charged`, `finishing charge`. Time is either `H:MM remaining` or
/// `(no estimate)` / absent. Returns `None` when no battery line exists
/// (desktop Macs print only the power-source line).
pub fn parse_pmset_batt(stdout: &str) -> Option<BatteryStatus> {
    let line = stdout.lines().find(|l| l.contains("InternalBattery"))?;

    let percent = line
        .split(';')
        .next()?
        .split_whitespace()
        .last()?
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()?;
    if !(0.0..=100.0).contains(&percent) {
        return None;
    }

    let mut parts = line.split(';');
    let _percent_part = parts.next();
    let state = normalize_state(parts.next()?.trim());
    let rest = parts.next().unwrap_or("");

    let minutes = parse_hh_mm(rest);
    let (to_empty, to_full) = match (state, minutes) {
        (BatteryState::Discharging, Some(s)) => (Some(s), None),
        (BatteryState::Charging, Some(s)) => (None, Some(s)),
        _ => (None, None),
    };

    Some(BatteryStatus {
        percent,
        state,
        time_to_empty_s: to_empty,
        time_to_full_s: to_full,
    })
}

fn normalize_state(raw: &str) -> BatteryState {
    match raw {
        "discharging" => BatteryState::Discharging,
        "charging" | "finishing charge" => BatteryState::Charging,
        "charged" => BatteryState::Full,
        _ => BatteryState::Unknown,
    }
}

/// `4:23 remaining` → seconds (26_100); `(no estimate)` → None.
fn parse_hh_mm(text: &str) -> Option<u64> {
    let token = text.split_whitespace().find(|t| t.contains(':'))?;
    let (h, m) = token.split_once(':')?;
    let h: u64 = h.trim().parse().ok()?;
    let m: u64 = m.trim().parse().ok()?;
    Some(h * 3600 + m * 60)
}

/// Parse `osascript -e 'get volume settings'` output:
/// `output volume:42, input volume:63, output muted:false, alert volume:100`
pub fn parse_volume_settings(stdout: &str) -> Option<VolumeStatus> {
    let text = stdout.trim();
    let percent = extract_kv(text, "output volume")?.parse::<u32>().ok()?;
    if percent > 100 {
        return None;
    }
    let muted = match extract_kv(text, "output muted")? {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    Some(VolumeStatus { percent, muted })
}

/// Value of `key:` in a comma-separated `key:value` list.
fn extract_kv<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.split(',').map(str::trim).find_map(|pair| {
        pair.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(str::trim)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON_BATTERY: &str = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=12345678)\t87%; discharging; 4:23 remaining present: true\n";

    const CHARGING: &str = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=12345678)\t42%; charging; 1:05 remaining present: true\n";

    const FULL_AC: &str = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=12345678)\t100%; charged; 0:00 remaining present: true\n";

    const NO_ESTIMATE: &str = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=98765432)\t55%; discharging; (no estimate) remaining present: true\n";

    #[test]
    fn parses_discharge_with_time_remaining() {
        let s = parse_pmset_batt(ON_BATTERY).expect("parses");
        assert_eq!(s.percent, 87.0);
        assert_eq!(s.state, BatteryState::Discharging);
        assert_eq!(s.time_to_empty_s, Some(4 * 3600 + 23 * 60));
        assert_eq!(s.time_to_full_s, None);
    }

    #[test]
    fn parses_charging_time_as_time_to_full() {
        let s = parse_pmset_batt(CHARGING).expect("parses");
        assert_eq!(s.state, BatteryState::Charging);
        assert_eq!(s.time_to_empty_s, None);
        assert_eq!(s.time_to_full_s, Some(60 * 60 + 5 * 60));
    }

    #[test]
    fn parses_charged_as_full_without_times() {
        let s = parse_pmset_batt(FULL_AC).expect("parses");
        assert_eq!(s.percent, 100.0);
        assert_eq!(s.state, BatteryState::Full);
        assert_eq!(s.time_to_empty_s, None);
        assert_eq!(s.time_to_full_s, None);
    }

    #[test]
    fn no_estimate_maps_to_none() {
        let s = parse_pmset_batt(NO_ESTIMATE).expect("parses");
        assert_eq!(s.state, BatteryState::Discharging);
        assert_eq!(s.time_to_empty_s, None);
    }

    #[test]
    fn desktop_mac_without_battery_is_none() {
        assert_eq!(parse_pmset_batt("Now drawing from 'AC Power'\n"), None);
        assert_eq!(parse_pmset_batt(""), None);
    }

    #[test]
    fn out_of_range_percent_rejected() {
        let bad = ON_BATTERY.replace("87%", "187%");
        assert_eq!(parse_pmset_batt(&bad), None);
    }

    const VOLUME: &str = "output volume:42, input volume:63, output muted:false, alert volume:100";
    const MUTED: &str = "output volume:0, input volume:22, output muted:true, alert volume:0";

    #[test]
    fn parses_volume_settings_both_mute_states() {
        assert_eq!(
            parse_volume_settings(VOLUME),
            Some(VolumeStatus {
                percent: 42,
                muted: false
            })
        );
        assert_eq!(
            parse_volume_settings(MUTED),
            Some(VolumeStatus {
                percent: 0,
                muted: true
            })
        );
    }

    #[test]
    fn volume_garbage_is_none() {
        assert_eq!(parse_volume_settings(""), None);
        assert_eq!(parse_volume_settings("output volume:abc"), None);
        assert_eq!(
            parse_volume_settings("output volume:142, output muted:false"),
            None
        );
        assert_eq!(parse_volume_settings("input volume:10"), None);
    }
}
