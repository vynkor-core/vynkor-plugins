//! Screen video record: single active-recording slot (mirrors `daemon`'s
//! one-busy-slot pattern), `max_duration_ms` safety cap enforced by a
//! spawned timeout task that SIGINTs the child so wf-recorder/ffmpeg
//! finalize a playable container instead of leaving a truncated file.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::error::CaptureError;
use crate::paths::{data_dir, record_filename};
use crate::screenshot::{parse as parse_region, Region};
use crate::session::{detect_session, SessionType};
use crate::spawner::{BoxedProcess, Spawner};

pub const DEFAULT_MAX_DURATION_MS: u64 = 1_800_000; // 30 minutes

struct ActiveRecording {
    id: String,
    path: std::path::PathBuf,
    process: BoxedProcess,
    started_at: std::time::Instant,
}

pub struct RecordState {
    inner: Mutex<Option<ActiveRecording>>,
}

impl RecordState {
    pub fn new() -> Self {
        Self { inner: Mutex::new(None) }
    }
}

impl Default for RecordState {
    fn default() -> Self {
        Self::new()
    }
}

fn record_args(session: SessionType, region: Region, path: &str) -> Option<(&'static str, Vec<String>)> {
    match session {
        SessionType::Wlroots => {
            let mut args = vec![];
            if let Region::Rect { x, y, w, h } = region {
                args.push("-g".to_string());
                args.push(format!("{x},{y} {w}x{h}"));
            }
            args.push("-f".to_string());
            args.push(path.to_string());
            Some(("wf-recorder", args))
        }
        SessionType::X11 => {
            let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
            let mut args = vec!["-f".into(), "x11grab".into()];
            // `-video_size` is an ffmpeg *input* option for x11grab and
            // must precede the `-i` it configures — placed after `-i` it
            // silently becomes an output option instead (rescale, not
            // crop), so it goes in this input-options block before `-i`.
            if let Region::Rect { w, h, .. } = region {
                args.push("-video_size".into());
                args.push(format!("{w}x{h}"));
            }
            let input = if let Region::Rect { x, y, .. } = region {
                format!("{display}+{x},{y}")
            } else {
                display
            };
            args.push("-i".into());
            args.push(input);
            args.push("-y".into());
            args.push(path.to_string());
            Some(("ffmpeg", args))
        }
        SessionType::Gnome | SessionType::Kde | SessionType::Unknown => None,
    }
}

pub async fn start(
    state: Arc<RecordState>,
    spawner: Arc<dyn Spawner>,
    params: &Value,
) -> Result<Value, CaptureError> {
    let mut guard = state.inner.lock().await;
    if guard.is_some() {
        return Err(CaptureError::Busy);
    }

    let region = parse_region(params)?;
    let max_duration_ms = params
        .get("max_duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_DURATION_MS);

    let session = detect_session(&|k| std::env::var(k).ok());
    let Some((bin, base_args)) = record_args(session, region, "") else {
        return Err(CaptureError::NotSupported("record"));
    };
    let ext = "mp4";
    let filename = record_filename(ext);
    let path = data_dir().join(&filename);
    let path_str = path.to_string_lossy().to_string();
    // `record_args` was called with an empty placeholder path (the real
    // path isn't known until `record_filename`/`data_dir` run, just
    // above) — every backend pushes that placeholder as one argv element,
    // so replace the empty string with the real path now.
    let args: Vec<String> = base_args.into_iter().map(|a| if a.is_empty() { path_str.clone() } else { a }).collect();

    let process = spawner.run_detached(bin, &args).await.map_err(|e| {
        if e.contains("ERR_CAPTURE_PROVIDER_MISSING") {
            CaptureError::NotSupported("record")
        } else {
            CaptureError::Backend(e)
        }
    })?;

    let id = format!("rec-{}", unix_millis());
    *guard = Some(ActiveRecording { id: id.clone(), path, process, started_at: std::time::Instant::now() });
    drop(guard);

    let timeout_state = Arc::clone(&state);
    let timeout_id = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(max_duration_ms)).await;
        let mut guard = timeout_state.inner.lock().await;
        if matches!(&*guard, Some(a) if a.id == timeout_id) {
            let active = guard.take().unwrap();
            drop(guard);
            let _ = finish(active).await;
        }
    });

    Ok(serde_json::json!({ "recording_id": id }))
}

pub async fn stop(state: &RecordState, params: &Value) -> Result<Value, CaptureError> {
    let recording_id = params
        .get("recording_id")
        .and_then(Value::as_str)
        .ok_or_else(|| CaptureError::BadParams("recording_id is required".into()))?;

    let mut guard = state.inner.lock().await;
    let Some(active) = guard.take() else {
        return Err(CaptureError::BadParams(format!("no active recording with id '{recording_id}'")));
    };
    if active.id != recording_id {
        *guard = Some(active);
        return Err(CaptureError::BadParams(format!("no active recording with id '{recording_id}'")));
    }
    drop(guard);

    finish(active).await
}

async fn finish(mut active: ActiveRecording) -> Result<Value, CaptureError> {
    send_sigint(&active.process);
    // Give the backend a moment to flush its container before falling
    // back to a hard kill — wf-recorder/ffmpeg both exit promptly on
    // SIGINT once they've finalized.
    for _ in 0..50 {
        if active.process.try_wait().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    active.process.start_kill();

    let duration_ms = active.started_at.elapsed().as_millis() as u64;
    Ok(serde_json::json!({
        "path": active.path.to_string_lossy(),
        "duration_ms": duration_ms,
    }))
}

fn send_sigint(process: &BoxedProcess) {
    if let Some(pid) = process.pid() {
        unsafe {
            libc::kill(pid as i32, libc::SIGINT);
        }
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawner::FakeSpawner;
    use std::sync::Mutex as StdMutex;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    #[tokio::test]
    async fn wlroots_record_args_full_region() {
        std::env::set_var("WAYLAND_DISPLAY", "wayland-1");
        std::env::set_var("XDG_CURRENT_DESKTOP", "Hyprland");
        let (bin, args) = record_args(SessionType::Wlroots, Region::Full, "/o.mp4").unwrap();
        assert_eq!(bin, "wf-recorder");
        assert_eq!(args, vec!["-f".to_string(), "/o.mp4".to_string()]);
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("XDG_CURRENT_DESKTOP");
    }

    #[test]
    fn x11_record_args_rect_region_puts_video_size_before_input() {
        std::env::set_var("DISPLAY", ":0");
        let (bin, args) = record_args(
            SessionType::X11,
            Region::Rect { x: 10, y: 20, w: 300, h: 400 },
            "/o.mp4",
        )
        .unwrap();
        assert_eq!(bin, "ffmpeg");
        assert_eq!(
            args,
            vec![
                "-f".to_string(),
                "x11grab".to_string(),
                "-video_size".to_string(),
                "300x400".to_string(),
                "-i".to_string(),
                ":0+10,20".to_string(),
                "-y".to_string(),
                "/o.mp4".to_string(),
            ]
        );
        std::env::remove_var("DISPLAY");
    }

    #[tokio::test]
    async fn start_rejects_second_recording_while_one_is_active() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("WAYLAND_DISPLAY", "wayland-1");
        std::env::set_var("XDG_CURRENT_DESKTOP", "Hyprland");
        let fake = Arc::new(FakeSpawner::new());
        fake.allow_detached("wf-recorder");
        let sp: Arc<dyn Spawner> = fake;
        let state = Arc::new(RecordState::new());
        let first = start(Arc::clone(&state), Arc::clone(&sp), &serde_json::json!({})).await;
        assert!(first.is_ok(), "{first:?}");
        let second = start(Arc::clone(&state), Arc::clone(&sp), &serde_json::json!({})).await;
        assert!(matches!(second.unwrap_err(), CaptureError::Busy));
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("XDG_CURRENT_DESKTOP");
    }

    #[tokio::test]
    async fn stop_with_unknown_id_is_bad_params() {
        let state = Arc::new(RecordState::new());
        let err = stop(&state, &serde_json::json!({"recording_id": "nope"})).await.unwrap_err();
        assert!(matches!(err, CaptureError::BadParams(_)));
    }

    #[tokio::test]
    async fn start_stop_round_trip_returns_path_and_duration() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("WAYLAND_DISPLAY", "wayland-1");
        std::env::set_var("XDG_CURRENT_DESKTOP", "Hyprland");
        let fake = FakeSpawner::new();
        fake.allow_detached("wf-recorder");
        let sp: Arc<dyn Spawner> = Arc::new(fake);
        let state = Arc::new(RecordState::new());
        let started = start(Arc::clone(&state), sp, &serde_json::json!({})).await.unwrap();
        let id = started["recording_id"].as_str().unwrap().to_string();
        let stopped = stop(&state, &serde_json::json!({"recording_id": id})).await.unwrap();
        assert!(stopped["path"].as_str().unwrap().ends_with(".mp4"));
        assert!(stopped["duration_ms"].as_u64().is_some());
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("XDG_CURRENT_DESKTOP");
    }

    #[test]
    fn not_supported_session_has_no_record_args() {
        assert!(record_args(SessionType::Gnome, Region::Full, "/o.mp4").is_none());
        assert!(record_args(SessionType::Kde, Region::Full, "/o.mp4").is_none());
    }
}
