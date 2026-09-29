//! One live camera stream at a time. A background task reads len32 JPEG
//! frames from the helper, atomically refreshes `latest.jpg` and (optionally)
//! appends to a record file. The helper's `--secs` deadline is the dead-man
//! switch that frees the camera even if the ssh child is killed abruptly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::error::{classify, PhoneError};
use crate::framing::{is_jpeg, read_frame};
use crate::helper::helper_cmd;
use crate::params::{Camera, StreamParams};
use crate::storage::{unix_millis, write_atomic};
use crate::transport::{Control, Transport};

#[derive(Default)]
struct Stats {
    frames: AtomicU64,
    /// Milliseconds since the stream started at the last frame; 0 = none yet.
    last_frame_ms: AtomicU64,
    done: AtomicBool,
}

struct Active {
    id: String,
    camera: Camera,
    width: u32,
    height: u32,
    started: Instant,
    stats: Arc<Stats>,
    control: Arc<Mutex<Box<dyn Control>>>,
    task: JoinHandle<()>,
    latest_path: PathBuf,
    record_path: Option<PathBuf>,
}

pub struct StreamRegistry {
    inner: Mutex<Option<Active>>,
    /// Serializes camera use: the HAL allows one client at a time.
    camera_lock: Mutex<()>,
}

impl Default for StreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub fn stream_args(p: &StreamParams) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--cam".into(),
        p.camera.as_str().into(),
        "-W".into(),
        p.width.to_string(),
        "-H".into(),
        p.height.to_string(),
        "--fps".into(),
        p.fps.to_string(),
        "--secs".into(),
        p.max_duration_ms.div_ceil(1000).to_string(),
        "--fmt".into(),
        "jpeg".into(),
        "--q".into(),
        p.quality.to_string(),
        "--af".into(),
        p.af.as_str().into(),
        "--framing".into(),
        "len32".into(),
        "--out".into(),
        "-".into(),
    ];
    if p.flash {
        a.extend(["--flash".to_string(), "3".to_string()]);
    }
    a
}

async fn pump(
    mut stdout: Box<dyn AsyncRead + Send + Unpin>,
    stats: Arc<Stats>,
    started: Instant,
    dir: PathBuf,
    record: Option<PathBuf>,
) {
    let mut rec = match record {
        Some(p) => tokio::fs::OpenOptions::new().create(true).append(true).open(p).await.ok(),
        None => None,
    };
    // Ends on clean EOF, a read error, or a bad frame header: all mean the helper is gone.
    while let Ok(Some(frame)) = read_frame(&mut stdout).await {
        if !is_jpeg(&frame) {
            continue;
        }
        if write_atomic(&dir, "latest.jpg", &frame).await.is_err() {
            break;
        }
        if let Some(f) = rec.as_mut() {
            if f.write_all(&frame).await.is_err() {
                rec = None;
            }
        }
        stats.frames.fetch_add(1, Ordering::SeqCst);
        stats.last_frame_ms.store((started.elapsed().as_millis() as u64).max(1), Ordering::SeqCst);
    }
    stats.done.store(true, Ordering::SeqCst);
}

impl StreamRegistry {
    pub fn new() -> Self {
        Self { inner: Mutex::new(None), camera_lock: Mutex::new(()) }
    }

    /// Hold this while talking to the camera for a single photo.
    pub async fn camera_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.camera_lock.lock().await
    }

    /// Drop a finished stream (helper exited / ssh dropped) so a new one can start.
    async fn reap(slot: &mut Option<Active>) {
        let finished = slot.as_ref().map(|a| a.stats.done.load(Ordering::SeqCst)).unwrap_or(false);
        if finished {
            if let Some(a) = slot.take() {
                a.control.lock().await.kill().await;
                let _ = a.task.await;
            }
        }
    }

    pub async fn start(&self, t: &dyn Transport, cfg: &Config, p: &StreamParams) -> Result<Value, PhoneError> {
        let mut slot = self.inner.lock().await;
        Self::reap(&mut slot).await;
        if let Some(a) = slot.as_ref() {
            return Err(PhoneError::Busy(format!("stream {} is already running (stop it first)", a.id)));
        }
        let _cam = self
            .camera_lock
            .try_lock()
            .map_err(|_| PhoneError::Busy("a photo is being taken right now".into()))?;
        cfg.ensure_dir()?;

        let spawned = t.spawn(&helper_cmd(cfg, stream_args(p))).await?;
        let started = Instant::now();
        let stats = Arc::new(Stats::default());
        let ms = unix_millis();
        let record_path = p.record.then(|| cfg.dir.join(format!("record-{ms}.mjpg")));
        let control = Arc::new(Mutex::new(spawned.control));
        let task = tokio::spawn(pump(
            spawned.stdout,
            Arc::clone(&stats),
            started,
            cfg.dir.clone(),
            record_path.clone(),
        ));
        let id = format!("s-{ms}");
        let latest_path = cfg.dir.join("latest.jpg");
        let out = json!({
            "stream_id": id,
            "latest_path": latest_path,
            "record_path": record_path,
            "camera": p.camera.as_str(),
            "width": p.width,
            "height": p.height,
        });
        *slot = Some(Active {
            id,
            camera: p.camera,
            width: p.width,
            height: p.height,
            started,
            stats,
            control,
            task,
            latest_path,
            record_path,
        });
        Ok(out)
    }

    pub async fn stop(&self, id: Option<String>) -> Result<Value, PhoneError> {
        let mut slot = self.inner.lock().await;
        if let (Some(want), Some(a)) = (id.as_ref(), slot.as_ref()) {
            if *want != a.id {
                return Err(PhoneError::BadParams(format!("no such stream '{want}' (active: {})", a.id)));
            }
        }
        let Some(a) = slot.take() else {
            return Ok(json!({"stopped": false, "frames": 0, "duration_ms": 0, "record_path": null}));
        };
        let stderr = {
            let mut c = a.control.lock().await;
            c.kill().await;
            c.stderr_tail()
        };
        if tokio::time::timeout(Duration::from_secs(2), a.task).await.is_err() {
            // pump ends when stdout closes; kill above guarantees that
        }
        let frames = a.stats.frames.load(Ordering::SeqCst);
        let mut out = json!({
            "stopped": true,
            "frames": frames,
            "duration_ms": a.started.elapsed().as_millis() as u64,
            "record_path": a.record_path,
        });
        if frames == 0 && !stderr.trim().is_empty() {
            // The helper never produced a frame: say why instead of a silent empty stream.
            out["error"] = json!(classify(1, &stderr).to_string());
        }
        Ok(out)
    }

    pub async fn status(&self) -> Value {
        let mut slot = self.inner.lock().await;
        let finished = slot.as_ref().map(|a| a.stats.done.load(Ordering::SeqCst)).unwrap_or(false);
        let Some(a) = slot.as_ref() else {
            return json!({"active": false});
        };
        let frames = a.stats.frames.load(Ordering::SeqCst);
        let last = a.stats.last_frame_ms.load(Ordering::SeqCst);
        let elapsed = a.started.elapsed().as_millis() as u64;
        let fps = if last > 0 { frames as f64 / (last as f64 / 1000.0) } else { 0.0 };
        let out = json!({
            "active": !finished,
            "stream_id": a.id,
            "camera": a.camera.as_str(),
            "width": a.width,
            "height": a.height,
            "frames": frames,
            "fps_measured": (fps * 10.0).round() / 10.0,
            "last_frame_age_ms": if last > 0 { Some(elapsed.saturating_sub(last)) } else { None },
            "latest_path": a.latest_path,
        });
        if finished {
            Self::reap(&mut slot).await;
        }
        out
    }

    pub async fn active_camera(&self) -> Option<(Camera, PathBuf)> {
        let mut slot = self.inner.lock().await;
        Self::reap(&mut slot).await;
        slot.as_ref().map(|a| (a.camera, a.latest_path.clone()))
    }

    pub async fn is_active(&self) -> bool {
        self.active_camera().await.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::encode_frame;
    use crate::transport::fake::FakeTransport;

    fn cfg(dir: &std::path::Path) -> Config {
        Config::from_lookup(|k| match k {
            "PHONE_PLUGIN_DIR" => Some(dir.to_string_lossy().into_owned()),
            _ => None,
        })
        .unwrap()
    }

    fn jpeg(tag: u8) -> Vec<u8> {
        vec![0xFF, 0xD8, tag, tag, 0xFF, 0xD9]
    }

    fn params() -> StreamParams {
        StreamParams::parse(&json!({"width":640,"height":480,"fps":10,"record":true})).unwrap()
    }

    async fn wait_frames(reg: &StreamRegistry, n: u64) {
        for _ in 0..100 {
            if reg.status().await["frames"].as_u64().unwrap_or(0) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("frames never reached {n}: {}", reg.status().await);
    }

    #[test]
    fn stream_args_shape() {
        let a = stream_args(&params());
        let s = a.join(" ");
        assert!(s.contains("--cam back -W 640 -H 480 --fps 10 --secs 300 --fmt jpeg --q 80 --af video --framing len32 --out -"), "{s}");
        assert!(!s.contains("--flash"));
        let f = stream_args(&StreamParams::parse(&json!({"flash":true,"max_duration_ms":1500})).unwrap());
        assert!(f.join(" ").contains("--flash 3"));
        assert!(f.join(" ").contains("--secs 2"), "1500 ms must round UP to 2 s: {}", f.join(" "));
    }

    #[tokio::test]
    async fn frames_update_latest_atomically_and_record_appends() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = encode_frame(&jpeg(1));
        bytes.extend(encode_frame(&jpeg(2)));
        let t = FakeTransport::new().with_stream(bytes, true);
        let reg = StreamRegistry::new();
        let started = reg.start(&t, &cfg(dir.path()), &params()).await.unwrap();
        assert!(started["stream_id"].as_str().unwrap().starts_with("s-"));
        wait_frames(&reg, 2).await;
        assert_eq!(std::fs::read(dir.path().join("latest.jpg")).unwrap(), jpeg(2));
        let rec = started["record_path"].as_str().unwrap();
        assert_eq!(std::fs::read(rec).unwrap(), [jpeg(1), jpeg(2)].concat());
        let st = reg.status().await;
        assert_eq!(st["active"], json!(true));
        assert_eq!(st["camera"], json!("back"));

        let stopped = reg.stop(None).await.unwrap();
        assert_eq!(stopped["stopped"], json!(true));
        assert_eq!(stopped["frames"], json!(2));
        assert!(t.killed.load(Ordering::SeqCst), "stop must kill the child (dead-man frees the camera)");
    }

    #[tokio::test]
    async fn second_start_is_busy_and_wrong_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let t = FakeTransport::new().with_stream(encode_frame(&jpeg(1)), true);
        let reg = StreamRegistry::new();
        reg.start(&t, &cfg(dir.path()), &params()).await.unwrap();
        assert!(matches!(reg.start(&t, &cfg(dir.path()), &params()).await, Err(PhoneError::Busy(_))));
        assert!(matches!(reg.stop(Some("s-nope".into())).await, Err(PhoneError::BadParams(_))));
        reg.stop(None).await.unwrap();
    }

    #[tokio::test]
    async fn stop_without_a_stream_is_a_clean_noop() {
        let reg = StreamRegistry::new();
        let r = reg.stop(None).await.unwrap();
        assert_eq!(r["stopped"], json!(false));
        assert_eq!(reg.status().await, json!({"active": false}));
        assert!(reg.active_camera().await.is_none());
    }

    #[tokio::test]
    async fn ssh_dropping_mid_stream_marks_it_inactive_and_allows_restart() {
        let dir = tempfile::tempdir().unwrap();
        // hold_open = false: stdout hits EOF right after the frame (connection dropped)
        let t = FakeTransport::new().with_stream(encode_frame(&jpeg(1)), false);
        let reg = StreamRegistry::new();
        reg.start(&t, &cfg(dir.path()), &params()).await.unwrap();
        for _ in 0..100 {
            if reg.status().await == json!({"active": false}) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // status reaped the dead stream; a new one may start and stop is clean
        assert_eq!(reg.status().await, json!({"active": false}));
        assert_eq!(reg.stop(None).await.unwrap()["stopped"], json!(false));
        let t2 = FakeTransport::new().with_stream(encode_frame(&jpeg(9)), true);
        reg.start(&t2, &cfg(dir.path()), &params()).await.unwrap();
        reg.stop(None).await.unwrap();
    }

    #[tokio::test]
    async fn non_jpeg_and_garbage_frames_are_not_published() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = encode_frame(b"GIF89a-not-a-jpeg");
        bytes.extend(encode_frame(&jpeg(5)));
        let t = FakeTransport::new().with_stream(bytes, true);
        let reg = StreamRegistry::new();
        reg.start(&t, &cfg(dir.path()), &params()).await.unwrap();
        wait_frames(&reg, 1).await;
        assert_eq!(std::fs::read(dir.path().join("latest.jpg")).unwrap(), jpeg(5));
        reg.stop(None).await.unwrap();
    }

    #[tokio::test]
    async fn spawned_command_targets_the_hashed_helper_with_camera_env() {
        let dir = tempfile::tempdir().unwrap();
        let t = FakeTransport::new().with_stream(Vec::new(), true);
        let reg = StreamRegistry::new();
        reg.start(&t, &cfg(dir.path()), &params()).await.unwrap();
        {
            let calls = t.calls.lock().unwrap();
            assert_eq!(calls[0].program, "python3");
            assert!(calls[0].args[0].contains("hybcam-"));
            assert!(calls[0].in_home);
        }
        reg.stop(None).await.unwrap();
    }
}
