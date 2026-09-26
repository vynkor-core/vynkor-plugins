//! Parse + validate weather requests.

pub const MAX_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_DAYS: u32 = 3;
pub const MAX_DAYS: u32 = 16;

/// Operator defaults from the environment: a home location so callers can
/// ask "what's the weather" without coordinates, plus the timezone and
/// HTTP timeout used when a call omits them.
#[derive(Debug, Clone)]
pub struct Defaults {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub timezone: String,
    pub timeout_ms: u64,
}

impl Default for Defaults {
    fn default() -> Self {
        Self { lat: None, lon: None, timezone: "auto".into(), timeout_ms: DEFAULT_TIMEOUT_MS }
    }
}

impl Defaults {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Unparseable or empty values fall back to the built-in default rather
    /// than failing startup — the per-call validation still applies.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite());
        let base = Self::default();
        Self {
            lat: num("WEATHER_PLUGIN_DEFAULT_LAT"),
            lon: num("WEATHER_PLUGIN_DEFAULT_LON"),
            timezone: get("WEATHER_PLUGIN_DEFAULT_TIMEZONE")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or(base.timezone),
            timeout_ms: get("WEATHER_PLUGIN_TIMEOUT_MS")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|v| *v > 0)
                .map(|v| v.min(MAX_TIMEOUT_MS))
                .unwrap_or(base.timeout_ms),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WeatherNowParams {
    pub lat: f64,
    pub lon: f64,
    pub timezone: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct WeatherForecastParams {
    pub lat: f64,
    pub lon: f64,
    pub days: u32,
    pub timezone: String,
    pub timeout_ms: u64,
}

/// Coordinates come as a pair: both from the caller, or — when the caller
/// gives neither — both from the operator's home location. Mixing one of
/// each would silently query a place nobody asked for.
fn resolve_lat_lon(
    lat: Option<serde_json::Value>,
    lon: Option<serde_json::Value>,
    defaults: &Defaults,
) -> Result<(f64, f64), String> {
    let (lat, lon) = match (lat, lon, defaults.lat, defaults.lon) {
        (None, None, Some(lat), Some(lon)) => (lat, lon),
        (None, None, _, _) => {
            return Err("missing required fields: lat, lon (or set WEATHER_PLUGIN_DEFAULT_LAT/WEATHER_PLUGIN_DEFAULT_LON for a home location)".into())
        }
        (lat, lon, _, _) => (
            lat.ok_or("missing required field: lat")?.as_f64().ok_or("lat must be a number")?,
            lon.ok_or("missing required field: lon")?.as_f64().ok_or("lon must be a number")?,
        ),
    };
    validate_lat_lon(lat, lon)?;
    Ok((lat, lon))
}

fn validate_lat_lon(lat: f64, lon: f64) -> Result<(), String> {
    if !(-90.0..=90.0).contains(&lat) {
        return Err(format!("lat out of range -90..90: {lat}"));
    }
    if !(-180.0..=180.0).contains(&lon) {
        return Err(format!("lon out of range -180..180: {lon}"));
    }
    if !lat.is_finite() || !lon.is_finite() {
        return Err("lat/lon must be finite numbers".into());
    }
    Ok(())
}

pub fn parse_now(params_json: &[u8], defaults: &Defaults) -> Result<WeatherNowParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        lat: Option<serde_json::Value>,
        lon: Option<serde_json::Value>,
        timezone: Option<String>,
        timeout_ms: Option<u64>,
    }
    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;
    let (lat, lon) = resolve_lat_lon(raw.lat, raw.lon, defaults)?;
    let timezone = raw.timezone.unwrap_or_else(|| defaults.timezone.clone());
    if timezone.trim().is_empty() {
        return Err("timezone must not be empty".into());
    }
    let timeout_ms = raw.timeout_ms.unwrap_or(defaults.timeout_ms).min(MAX_TIMEOUT_MS);
    if timeout_ms == 0 {
        return Err("timeout_ms must be > 0".into());
    }
    Ok(WeatherNowParams {
        lat,
        lon,
        timezone,
        timeout_ms,
    })
}

pub fn parse_forecast(params_json: &[u8], defaults: &Defaults) -> Result<WeatherForecastParams, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        lat: Option<serde_json::Value>,
        lon: Option<serde_json::Value>,
        days: Option<u32>,
        timezone: Option<String>,
        timeout_ms: Option<u64>,
    }
    let raw: Raw = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;
    let (lat, lon) = resolve_lat_lon(raw.lat, raw.lon, defaults)?;
    let days = raw.days.unwrap_or(DEFAULT_DAYS);
    if !(1..=MAX_DAYS).contains(&days) {
        return Err(format!("days out of range 1..{MAX_DAYS}: {days}"));
    }
    let timezone = raw.timezone.unwrap_or_else(|| defaults.timezone.clone());
    if timezone.trim().is_empty() {
        return Err("timezone must not be empty".into());
    }
    let timeout_ms = raw.timeout_ms.unwrap_or(defaults.timeout_ms).min(MAX_TIMEOUT_MS);
    if timeout_ms == 0 {
        return Err("timeout_ms must be > 0".into());
    }
    Ok(WeatherForecastParams {
        lat,
        lon,
        days,
        timezone,
        timeout_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_accepts_valid() {
        let p = parse_now(br#"{"lat": 52.5, "lon": 13.4}"#, &Defaults::default()).unwrap();
        assert_eq!(p.lat, 52.5);
        assert_eq!(p.timezone, "auto");
    }

    #[test]
    fn now_rejects_bad_lat() {
        let e = parse_now(br#"{"lat": 100, "lon": 0}"#, &Defaults::default()).unwrap_err();
        assert!(e.contains("lat"), "{e}");
    }

    #[test]
    fn forecast_clamps_days() {
        let p = parse_forecast(br#"{"lat": 0, "lon": 0, "days": 5}"#, &Defaults::default()).unwrap();
        assert_eq!(p.days, 5);
    }

    #[test]
    fn forecast_rejects_days_oob() {
        let e = parse_forecast(br#"{"lat": 0, "lon": 0, "days": 20}"#, &Defaults::default()).unwrap_err();
        assert!(e.contains("days"), "{e}");
    }

    #[test]
    fn missing_lat() {
        let e = parse_now(br#"{"lon": 0}"#, &Defaults::default()).unwrap_err();
        assert!(e.contains("lat"), "{e}");
    }

    fn home() -> Defaults {
        Defaults { lat: Some(41.3), lon: Some(69.24), timezone: "Asia/Tashkent".into(), timeout_ms: 4000 }
    }

    #[test]
    fn omitted_coords_fall_back_to_home_location() {
        let p = parse_now(b"{}", &home()).unwrap();
        assert_eq!((p.lat, p.lon), (41.3, 69.24));
        assert_eq!(p.timezone, "Asia/Tashkent");
        assert_eq!(p.timeout_ms, 4000);
        let f = parse_forecast(br#"{"days": 1}"#, &home()).unwrap();
        assert_eq!((f.lat, f.lon, f.days), (41.3, 69.24, 1));
    }

    #[test]
    fn explicit_params_beat_defaults() {
        let p = parse_now(br#"{"lat": 1, "lon": 2, "timezone": "UTC", "timeout_ms": 500}"#, &home()).unwrap();
        assert_eq!((p.lat, p.lon, p.timezone.as_str(), p.timeout_ms), (1.0, 2.0, "UTC", 500));
    }

    #[test]
    fn half_given_coords_do_not_mix_with_home() {
        // lat from the caller + lon from the config would be a place nobody asked for
        let e = parse_now(br#"{"lat": 10}"#, &home()).unwrap_err();
        assert!(e.contains("lon"), "{e}");
    }

    #[test]
    fn missing_coords_without_home_names_the_env_vars() {
        let e = parse_now(b"{}", &Defaults::default()).unwrap_err();
        assert!(e.contains("WEATHER_PLUGIN_DEFAULT_LAT"), "{e}");
    }

    #[test]
    fn defaults_parse_from_env_lookup() {
        let env = |k: &str| match k {
            "WEATHER_PLUGIN_DEFAULT_LAT" => Some("41.3".to_string()),
            "WEATHER_PLUGIN_DEFAULT_LON" => Some("69.24".to_string()),
            "WEATHER_PLUGIN_TIMEOUT_MS" => Some("99999".to_string()),
            _ => None,
        };
        let d = Defaults::from_lookup(env);
        assert_eq!((d.lat, d.lon), (Some(41.3), Some(69.24)));
        assert_eq!(d.timezone, "auto");
        assert_eq!(d.timeout_ms, MAX_TIMEOUT_MS, "clamped like the per-call value");
    }
}
