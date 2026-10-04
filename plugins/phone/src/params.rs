//! Strict request parsing for the camera actions. Unknown keys, wrong JSON
//! types and off-table values are `ERR_PHONE_BAD_PARAMS` — nothing reaches a
//! command line unvalidated.

use serde_json::{Map, Value};

use crate::error::PhoneError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Camera {
    Back,
    Front,
}

impl Camera {
    pub fn as_str(self) -> &'static str {
        match self {
            Camera::Back => "back",
            Camera::Front => "front",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Af {
    Video,
    Picture,
    Auto,
    Off,
}

impl Af {
    pub fn as_str(self) -> &'static str {
        match self {
            Af::Video => "video",
            Af::Picture => "picture",
            Af::Auto => "auto",
            Af::Off => "off",
        }
    }
}

/// Preview sizes the device enumerates and we allow.
pub const SIZES: [(u32, u32); 5] = [(1920, 1080), (1280, 720), (800, 600), (640, 480), (320, 240)];
pub const DEFAULT_SIZE: (u32, u32) = (1280, 720);
pub const MAX_DURATION_CAP_MS: u64 = 1_800_000;
pub const DEFAULT_DURATION_MS: u64 = 300_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoParams {
    pub camera: Camera,
    pub width: u32,
    pub height: u32,
    pub af: Af,
    pub flash: bool,
    pub quality: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamParams {
    pub camera: Camera,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub quality: u8,
    pub af: Af,
    pub flash: bool,
    pub record: bool,
    pub max_duration_ms: u64,
}

fn bad(msg: impl Into<String>) -> PhoneError {
    PhoneError::BadParams(msg.into())
}

/// `null` / absent params behave like `{}`; anything else must be an object
/// whose keys are all in `allowed`.
fn object(v: &Value, allowed: &[&str]) -> Result<Map<String, Value>, PhoneError> {
    let m = match v {
        Value::Null => Map::new(),
        Value::Object(m) => m.clone(),
        _ => return Err(bad("params must be a JSON object")),
    };
    if let Some(k) = m.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(bad(format!("unknown parameter '{k}'")));
    }
    Ok(m)
}

fn camera(m: &Map<String, Value>) -> Result<Camera, PhoneError> {
    match m.get("camera") {
        None => Ok(Camera::Back),
        Some(Value::String(s)) if s == "back" => Ok(Camera::Back),
        Some(Value::String(s)) if s == "front" => Ok(Camera::Front),
        Some(_) => Err(bad("camera must be 'back' or 'front'")),
    }
}

fn af(m: &Map<String, Value>) -> Result<Af, PhoneError> {
    match m.get("af") {
        None => Ok(Af::Video),
        Some(Value::String(s)) => match s.as_str() {
            "video" => Ok(Af::Video),
            "picture" => Ok(Af::Picture),
            "auto" => Ok(Af::Auto),
            "off" => Ok(Af::Off),
            _ => Err(bad("af must be one of video|picture|auto|off")),
        },
        Some(_) => Err(bad("af must be a string")),
    }
}

fn boolean(m: &Map<String, Value>, key: &str) -> Result<bool, PhoneError> {
    match m.get(key) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad(format!("{key} must be a boolean"))),
    }
}

fn int_in(m: &Map<String, Value>, key: &str, min: u64, max: u64, default: u64) -> Result<u64, PhoneError> {
    match m.get(key) {
        None => Ok(default),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| bad(format!("{key} must be a non-negative integer")))?;
            if n < min || n > max {
                return Err(bad(format!("{key} must be within {min}..={max}")));
            }
            Ok(n)
        }
    }
}

fn size(m: &Map<String, Value>) -> Result<(u32, u32), PhoneError> {
    match (m.get("width"), m.get("height")) {
        (None, None) => Ok(DEFAULT_SIZE),
        (Some(w), Some(h)) => {
            let w = w.as_u64().ok_or_else(|| bad("width must be an integer"))?;
            let h = h.as_u64().ok_or_else(|| bad("height must be an integer"))?;
            SIZES
                .iter()
                .find(|(sw, sh)| u64::from(*sw) == w && u64::from(*sh) == h)
                .copied()
                .ok_or_else(|| bad(format!("{w}x{h} is not a supported size (see plugin.json)")))
        }
        _ => Err(bad("width and height must be given together")),
    }
}

impl PhotoParams {
    pub fn parse(v: &Value) -> Result<Self, PhoneError> {
        let m = object(v, &["camera", "width", "height", "af", "flash", "quality"])?;
        let (width, height) = size(&m)?;
        Ok(Self {
            camera: camera(&m)?,
            width,
            height,
            af: af(&m)?,
            flash: boolean(&m, "flash")?,
            quality: int_in(&m, "quality", 30, 95, 80)? as u8,
        })
    }
}

impl StreamParams {
    pub fn parse(v: &Value) -> Result<Self, PhoneError> {
        let m = object(
            v,
            &["camera", "width", "height", "fps", "quality", "af", "flash", "record", "max_duration_ms"],
        )?;
        let (width, height) = size(&m)?;
        Ok(Self {
            camera: camera(&m)?,
            width,
            height,
            fps: int_in(&m, "fps", 1, 30, 15)? as u32,
            quality: int_in(&m, "quality", 30, 95, 80)? as u8,
            af: af(&m)?,
            flash: boolean(&m, "flash")?,
            record: boolean(&m, "record")?,
            max_duration_ms: int_in(&m, "max_duration_ms", 1000, MAX_DURATION_CAP_MS, DEFAULT_DURATION_MS)?,
        })
    }
}

/// `phone_stream_stop` params: optional `stream_id` (a non-empty string).
pub fn parse_stream_id(v: &Value) -> Result<Option<String>, PhoneError> {
    let m = object(v, &["stream_id"])?;
    match m.get("stream_id") {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 64 => Ok(Some(s.clone())),
        Some(_) => Err(bad("stream_id must be a non-empty string (<= 64 bytes)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn photo_defaults() {
        let p = PhotoParams::parse(&json!({})).unwrap();
        assert_eq!(
            p,
            PhotoParams { camera: Camera::Back, width: 1280, height: 720, af: Af::Video, flash: false, quality: 80 }
        );
        assert_eq!(PhotoParams::parse(&Value::Null).unwrap(), p);
    }

    #[test]
    fn photo_full_override() {
        let p = PhotoParams::parse(
            &json!({"camera":"front","width":640,"height":480,"af":"auto","flash":true,"quality":50}),
        )
        .unwrap();
        assert_eq!(p.camera, Camera::Front);
        assert_eq!((p.width, p.height), (640, 480));
        assert_eq!(p.af, Af::Auto);
        assert!(p.flash);
        assert_eq!(p.quality, 50);
    }

    #[test]
    fn rejects_hostile_and_malformed_photo_params() {
        let cases = [
            json!({"camera":"side"}),
            json!({"camera":1}),
            json!({"width":1280}),
            json!({"height":720}),
            json!({"width":123,"height":45}),
            json!({"width":"1280","height":"720"}),
            json!({"af":"; rm -rf /"}),
            json!({"af":true}),
            json!({"flash":"yes"}),
            json!({"quality":10}),
            json!({"quality":96}),
            json!({"quality":-1}),
            json!({"quality":50.5}),
            json!({"nope":1}),
            json!([1, 2]),
            json!("str"),
            json!(5),
        ];
        for c in cases {
            assert!(
                matches!(PhotoParams::parse(&c), Err(PhoneError::BadParams(_))),
                "must reject {c}"
            );
        }
    }

    #[test]
    fn stream_defaults_and_bounds() {
        let s = StreamParams::parse(&json!({})).unwrap();
        assert_eq!(s.fps, 15);
        assert_eq!(s.max_duration_ms, 300_000);
        assert!(!s.record);
        assert!(StreamParams::parse(&json!({"fps":0})).is_err());
        assert!(StreamParams::parse(&json!({"fps":31})).is_err());
        assert!(StreamParams::parse(&json!({"max_duration_ms":999})).is_err());
        assert!(StreamParams::parse(&json!({"max_duration_ms":1_800_001})).is_err());
        assert!(StreamParams::parse(&json!({"max_duration_ms":1_800_000})).is_ok());
        assert!(StreamParams::parse(&json!({"record":"true"})).is_err());
    }

    #[test]
    fn stream_id_parsing() {
        assert_eq!(parse_stream_id(&json!({})).unwrap(), None);
        assert_eq!(parse_stream_id(&json!({"stream_id":"s-1"})).unwrap(), Some("s-1".into()));
        assert!(parse_stream_id(&json!({"stream_id":""})).is_err());
        assert!(parse_stream_id(&json!({"stream_id":5})).is_err());
        assert!(parse_stream_id(&json!({"stream_id":"x".repeat(65)})).is_err());
        assert!(parse_stream_id(&json!({"other":1})).is_err());
    }
}
