//! Screenshot backend chain: try each candidate for the detected session
//! in order, falling through to the next on
//! `ERR_CAPTURE_PROVIDER_MISSING`. Once a binary is found, its exit code
//! is authoritative — a nonzero exit (or a cancelled interactive
//! selection) is a terminal error, not a reason to keep trying.

use serde_json::Value;

use crate::error::CaptureError;
use crate::paths::{data_dir, screenshot_filename};
use crate::session::{detect_session, SessionType};
use crate::spawner::Spawner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Full,
    Select,
    Rect { x: i32, y: i32, w: u32, h: u32 },
}

pub fn parse(v: &Value) -> Result<Region, CaptureError> {
    let region = match v.get("region") {
        None | Some(Value::Null) => return Ok(Region::Full),
        Some(Value::String(s)) if s == "full" => return Ok(Region::Full),
        Some(Value::String(s)) if s == "select" => return Ok(Region::Select),
        Some(Value::String(s)) => {
            return Err(CaptureError::BadParams(format!("unknown region string '{s}'")))
        }
        Some(obj) => obj,
    };
    let field = |k: &str| -> Result<i64, CaptureError> {
        region
            .get(k)
            .and_then(Value::as_i64)
            .ok_or_else(|| CaptureError::BadParams(format!("region.{k} must be an integer")))
    };
    let x = field("x")?;
    let y = field("y")?;
    let w = field("w")?;
    let h = field("h")?;
    if w <= 0 || h <= 0 {
        return Err(CaptureError::BadParams("region.w and region.h must be positive".into()));
    }
    Ok(Region::Rect { x: x as i32, y: y as i32, w: w as u32, h: h as u32 })
}

/// One screenshot candidate: binary name + argv builder given the target
/// path. `None` from the builder means "this backend can't express this
/// region" (falls through with a `warning`, not an error).
struct Candidate {
    bin: &'static str,
    build: fn(Region, &str) -> Option<Vec<String>>,
    /// True when this candidate needs stdout captured first (`slurp`) to
    /// build the real command's args — handled specially in the runner.
    needs_slurp: bool,
}

fn grim_args(region: Region, path: &str) -> Option<Vec<String>> {
    match region {
        Region::Full => Some(vec![path.to_string()]),
        Region::Rect { x, y, w, h } => {
            Some(vec!["-g".to_string(), format!("{x},{y} {w}x{h}"), path.to_string()])
        }
        Region::Select => None, // built specially: slurp output feeds -g
    }
}

fn gnome_screenshot_args(region: Region, path: &str) -> Option<Vec<String>> {
    match region {
        Region::Full => Some(vec!["-f".to_string(), path.to_string()]),
        Region::Select => Some(vec!["-a".to_string(), "-f".to_string(), path.to_string()]),
        Region::Rect { .. } => None, // no geometry flag on this CLI
    }
}

fn spectacle_args(region: Region, path: &str) -> Option<Vec<String>> {
    match region {
        Region::Full => Some(vec!["-b".into(), "-n".into(), "-o".into(), path.to_string()]),
        Region::Select => Some(vec!["-b".into(), "-n".into(), "-r".into(), "-o".into(), path.to_string()]),
        Region::Rect { .. } => None,
    }
}

fn maim_args(region: Region, path: &str) -> Option<Vec<String>> {
    match region {
        Region::Full => Some(vec![path.to_string()]),
        Region::Select => Some(vec!["-s".into(), path.to_string()]),
        Region::Rect { x, y, w, h } => {
            Some(vec!["-g".into(), format!("{w}x{h}+{x}+{y}"), path.to_string()])
        }
    }
}

fn scrot_args(region: Region, path: &str) -> Option<Vec<String>> {
    match region {
        Region::Full => Some(vec![path.to_string()]),
        _ => None, // scrot's geometry/select flags are unreliable cross-version; skip
    }
}

fn import_args(region: Region, path: &str) -> Option<Vec<String>> {
    match region {
        Region::Full => Some(vec!["-window".into(), "root".into(), path.to_string()]),
        Region::Select => Some(vec![path.to_string()]), // no -window = interactive crosshair select
        Region::Rect { x, y, w, h } => {
            Some(vec!["-window".into(), "root".into(), "-crop".into(), format!("{w}x{h}+{x}+{y}"), path.to_string()])
        }
    }
}

fn candidates_for(session: SessionType) -> &'static [Candidate] {
    match session {
        SessionType::Wlroots => &[Candidate { bin: "grim", build: grim_args, needs_slurp: true }],
        SessionType::Gnome => &[Candidate { bin: "gnome-screenshot", build: gnome_screenshot_args, needs_slurp: false }],
        SessionType::Kde => &[Candidate { bin: "spectacle", build: spectacle_args, needs_slurp: false }],
        SessionType::X11 | SessionType::Unknown => &[
            Candidate { bin: "maim", build: maim_args, needs_slurp: false },
            Candidate { bin: "scrot", build: scrot_args, needs_slurp: false },
            Candidate { bin: "import", build: import_args, needs_slurp: false },
        ],
    }
}

pub async fn capture_screenshot(spawner: &dyn Spawner, params: &Value) -> Result<Value, CaptureError> {
    let region = parse(params)?;
    let ext = "png";
    let filename = screenshot_filename(ext);
    let path = data_dir().join(&filename);
    let path_str = path.to_string_lossy().to_string();

    let session = detect_session(&|k| std::env::var(k).ok());

    for cand in candidates_for(session) {
        if region == Region::Select && cand.needs_slurp {
            // grim's chain: slurp draws the box, grim consumes -g <geom>.
            match spawner.run_capturing("slurp", &[]).await {
                Ok((0, geom)) if !geom.is_empty() => {
                    let args = vec!["-g".to_string(), geom, path_str.clone()];
                    match run_candidate(spawner, cand.bin, args, ext, &path, &path_str).await {
                        // slurp exists but grim itself doesn't (or vice
                        // versa's binary went missing between probes) —
                        // fall through to the rest of the chain / the
                        // portal, same as every other candidate below,
                        // instead of giving up on the whole call. Since
                        // `candidates_for(Wlroots)` only ever has this one
                        // candidate, `continue` simply falls out of the
                        // loop and lands on the post-loop portal call.
                        Err(CaptureError::NotSupported(_)) => continue,
                        other => return other,
                    }
                }
                Ok(_) => return Err(CaptureError::Cancelled("slurp")),
                Err(e) if e.contains("ERR_CAPTURE_PROVIDER_MISSING") => continue,
                Err(e) => return Err(CaptureError::Backend(e)),
            }
        }

        let Some(args) = (cand.build)(region, &path_str) else {
            continue; // this backend can't express the requested region
        };
        match run_candidate(spawner, cand.bin, args, ext, &path, &path_str).await {
            // The binary for this candidate isn't installed — keep trying
            // the rest of the chain instead of giving up on the whole call.
            Err(CaptureError::NotSupported(_)) => continue,
            other => return other,
        }
    }

    let interactive = region != Region::Full;
    crate::portal::screenshot_via_portal(interactive, &path).await?;
    let (width, height) = image_dimensions_best_effort(&path);
    Ok(serde_json::json!({
        "path": path_str,
        "width": width,
        "height": height,
        "format": ext,
    }))
}

/// Spawn one candidate binary and turn its outcome into the call's result:
/// a missing binary surfaces as `NotSupported` so the caller can fall
/// through to the next candidate; once the binary *is* found, its exit
/// code is authoritative (nonzero -> `Cancelled`, not a fallthrough).
async fn run_candidate(
    spawner: &dyn Spawner,
    bin: &'static str,
    args: Vec<String>,
    ext: &str,
    path: &std::path::Path,
    path_str: &str,
) -> Result<Value, CaptureError> {
    let (code, _stdout) = spawner.run_capturing(bin, &args).await.map_err(|e| {
        if e.contains("ERR_CAPTURE_PROVIDER_MISSING") {
            CaptureError::NotSupported("screenshot")
        } else {
            CaptureError::Backend(e)
        }
    })?;
    if code != 0 {
        return Err(CaptureError::Cancelled(bin));
    }
    let (width, height) = image_dimensions_best_effort(path);
    Ok(serde_json::json!({
        "path": path_str,
        "width": width,
        "height": height,
        "format": ext,
    }))
}

/// Best-effort PNG dimension read (8-byte signature + 13-byte IHDR chunk,
/// width/height are the first 8 bytes of IHDR's data, big-endian) — avoids
/// pulling an image-decoding crate for two integers. Returns 0/0 (and
/// still succeeds the call) if the file is missing/malformed, e.g. under
/// a `FakeSpawner` test that never wrote a real file.
fn image_dimensions_best_effort(path: &std::path::Path) -> (u32, u32) {
    let Ok(bytes) = std::fs::read(path) else { return (0, 0) };
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return (0, 0);
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ENV_LOCK;

    #[test]
    fn parse_defaults_to_full() {
        assert_eq!(parse(&serde_json::json!({})).unwrap(), Region::Full);
    }

    #[test]
    fn parse_select_string() {
        assert_eq!(parse(&serde_json::json!({"region": "select"})).unwrap(), Region::Select);
    }

    #[test]
    fn parse_rect_object() {
        let r = parse(&serde_json::json!({"region": {"x": 1, "y": 2, "w": 300, "h": 400}})).unwrap();
        assert_eq!(r, Region::Rect { x: 1, y: 2, w: 300, h: 400 });
    }

    #[test]
    fn parse_rejects_zero_size_rect() {
        let err = parse(&serde_json::json!({"region": {"x": 0, "y": 0, "w": 0, "h": 10}})).unwrap_err();
        assert!(matches!(err, CaptureError::BadParams(_)));
    }

    #[test]
    fn grim_args_full_and_rect() {
        assert_eq!(grim_args(Region::Full, "/o.png"), Some(vec!["/o.png".to_string()]));
        assert_eq!(
            grim_args(Region::Rect { x: 1, y: 2, w: 3, h: 4 }, "/o.png"),
            Some(vec!["-g".to_string(), "1,2 3x4".to_string(), "/o.png".to_string()])
        );
    }

    #[test]
    fn maim_select_uses_dash_s() {
        assert_eq!(maim_args(Region::Select, "/o.png"), Some(vec!["-s".to_string(), "/o.png".to_string()]));
    }

    #[test]
    fn gnome_screenshot_has_no_rect_support() {
        assert_eq!(gnome_screenshot_args(Region::Rect { x: 0, y: 0, w: 1, h: 1 }, "/o.png"), None);
    }

    /// `detect_session` also branches on `XDG_SESSION_TYPE` and
    /// `XDG_CURRENT_DESKTOP` (see `session.rs`); a real desktop session
    /// (e.g. this repo's own dev box, which runs Hyprland under Wayland)
    /// has both set in the ambient process env. Each of these three tests
    /// wants a deterministic session, so all Wayland-signaling vars are
    /// cleared before setting up the scenario it actually wants to test —
    /// not just `WAYLAND_DISPLAY` as the upstream brief's snippet assumed.
    fn clear_session_env() {
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("XDG_SESSION_TYPE");
        std::env::remove_var("XDG_CURRENT_DESKTOP");
        std::env::remove_var("DISPLAY");
    }

    #[tokio::test]
    async fn falls_through_from_maim_to_scrot_on_x11() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_session_env();
        std::env::set_var("DISPLAY", ":0");
        let sp = crate::spawner::FakeSpawner::new();
        sp.set_capturing("scrot", 0, "");
        let v = capture_screenshot(&sp, &serde_json::json!({})).await.unwrap();
        assert!(v["path"].as_str().unwrap().ends_with(".png"));
        let calls = sp.capturing_calls.lock().unwrap().clone();
        assert_eq!(calls[0].0, "maim");
        assert_eq!(calls[1].0, "scrot");
        std::env::remove_var("DISPLAY");
    }

    #[tokio::test]
    async fn cancelled_selection_is_terminal_not_fallthrough() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_session_env();
        std::env::set_var("DISPLAY", ":0");
        let sp = crate::spawner::FakeSpawner::new();
        sp.set_capturing("maim", 1, ""); // maim found, but -s was Esc'd -> nonzero exit
        let err = capture_screenshot(&sp, &serde_json::json!({"region": "select"})).await.unwrap_err();
        assert!(matches!(err, CaptureError::Cancelled("maim")), "{err:?}");
        std::env::remove_var("DISPLAY");
    }

    #[tokio::test]
    #[ignore = "now reaches the live xdg-desktop-portal Screenshot fallback added in Task 7 — not mockable via the Spawner trait, and unsafe to run unattended (real D-Bus call, real interactive dialog on hosts with a portal). See portal.rs's URI-parsing unit tests for the coverage boundary."]
    async fn not_supported_when_no_backend_present() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_session_env();
        let sp = crate::spawner::FakeSpawner::new();
        let err = capture_screenshot(&sp, &serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, CaptureError::NotSupported("screenshot")));
    }
}
