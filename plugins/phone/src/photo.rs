//! `phone_photo`: one still frame. Runs the helper with `--snap` (autofocus
//! settles, one frame out), validates the JPEG, writes `photo-<ms>.jpg`.
//! While a stream owns the camera, returns the newest stream frame instead.

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{classify, PhoneError};
use crate::framing::{is_jpeg, read_frame};
use crate::helper::helper_cmd;
use crate::params::PhotoParams;
use crate::storage::{unix_millis, write_atomic};
use crate::stream::StreamRegistry;
use crate::transport::Transport;

const PHOTO_TIMEOUT: Duration = Duration::from_secs(15);

pub fn photo_args(p: &PhotoParams) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--cam".into(),
        p.camera.as_str().into(),
        "-W".into(),
        p.width.to_string(),
        "-H".into(),
        p.height.to_string(),
        "--fps".into(),
        "30".into(),
        "--secs".into(),
        "12".into(),
        "--fmt".into(),
        "jpeg".into(),
        "--q".into(),
        p.quality.to_string(),
        "--af".into(),
        p.af.as_str().into(),
        "--snap".into(),
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

pub async fn take(t: &dyn Transport, cfg: &Config, reg: &StreamRegistry, p: &PhotoParams) -> Result<Value, PhoneError> {
    cfg.ensure_dir()?;

    if let Some((cam, latest)) = reg.active_camera().await {
        if cam != p.camera {
            return Err(PhoneError::Busy(format!(
                "a {} stream owns the camera; stop it or ask for the {} camera",
                cam.as_str(),
                cam.as_str()
            )));
        }
        let bytes = tokio::fs::read(&latest)
            .await
            .map_err(|_| PhoneError::Busy("stream is running but has produced no frame yet; retry".into()))?;
        let path = write_atomic(&cfg.dir, &format!("photo-{}.jpg", unix_millis()), &bytes).await?;
        return Ok(json!({
            "path": path, "width": p.width, "height": p.height, "format": "jpg",
            "camera": p.camera.as_str(), "source": "stream", "bytes": bytes.len(),
        }));
    }

    // One camera client at a time: a second concurrent photo waits here.
    let _cam = reg.camera_guard().await;
    let out = t.run(&helper_cmd(cfg, photo_args(p)), None, PHOTO_TIMEOUT).await?;
    if out.code != 0 {
        return Err(classify(out.code, &out.stderr));
    }
    let mut r = &out.stdout[..];
    let frame = read_frame(&mut r)
        .await?
        .ok_or_else(|| PhoneError::Camera("helper exited without a frame".into()))?;
    if !is_jpeg(&frame) {
        return Err(PhoneError::Camera("helper returned bytes that are not a JPEG".into()));
    }
    let path = write_atomic(&cfg.dir, &format!("photo-{}.jpg", unix_millis()), &frame).await?;
    Ok(json!({
        "path": path, "width": p.width, "height": p.height, "format": "jpg",
        "camera": p.camera.as_str(), "source": "camera", "bytes": frame.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{encode_frame, MAX_FRAME};
    use crate::transport::fake::FakeTransport;
    use crate::transport::Output;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn cfg(dir: &std::path::Path) -> Config {
        Config::from_lookup(|k| match k {
            "PHONE_PLUGIN_DIR" => Some(dir.to_string_lossy().into_owned()),
            _ => None,
        })
        .unwrap()
    }

    fn jpeg() -> Vec<u8> {
        vec![0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9]
    }

    fn params() -> PhotoParams {
        PhotoParams::parse(&json!({})).unwrap()
    }

    fn files(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn photo_args_shape() {
        let s = photo_args(&params()).join(" ");
        assert_eq!(
            s,
            "--cam back -W 1280 -H 720 --fps 30 --secs 12 --fmt jpeg --q 80 --af video --snap --framing len32 --out -"
        );
        let f = PhotoParams::parse(&json!({"camera":"front","flash":true,"af":"auto"})).unwrap();
        let s = photo_args(&f).join(" ");
        assert!(s.contains("--cam front") && s.contains("--af auto") && s.ends_with("--flash 3"), "{s}");
    }

    #[tokio::test]
    async fn happy_path_writes_the_jpeg_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(encode_frame(&jpeg())));
        let reg = StreamRegistry::new();
        let v = take(&t, &cfg(dir.path()), &reg, &params()).await.unwrap();
        let path = v["path"].as_str().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), jpeg());
        assert_eq!(v["source"], json!("camera"));
        assert_eq!(v["format"], json!("jpg"));
        assert_eq!(v["bytes"], json!(7));
        assert!(std::path::Path::new(path).file_name().unwrap().to_string_lossy().starts_with("photo-"));
    }

    #[tokio::test]
    async fn garbage_truncated_and_oversized_output_writes_no_file() {
        let cases: Vec<Vec<u8>> = vec![
            encode_frame(b"not a jpeg at all"),
            vec![0, 0, 0, 9, 0xFF, 0xD8],
            vec![0, 0],
            Vec::new(),
            ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec(),
            vec![0, 0, 0, 0],
        ];
        for bytes in cases {
            let dir = tempfile::tempdir().unwrap();
            let t = FakeTransport::new();
            t.push_run(FakeTransport::ok(bytes.clone()));
            let reg = StreamRegistry::new();
            let e = take(&t, &cfg(dir.path()), &reg, &params()).await.unwrap_err();
            assert!(matches!(e, PhoneError::Camera(_)), "{bytes:?} -> {e}");
            assert!(files(dir.path()).is_empty(), "{bytes:?} left files: {:?}", files(dir.path()));
        }
    }

    #[tokio::test]
    async fn helper_failures_map_to_the_taxonomy() {
        let dir = tempfile::tempdir().unwrap();
        let reg = StreamRegistry::new();
        for (code, stderr, want) in [
            (255, "ssh: connect: no route", "ERR_PHONE_UNREACHABLE"),
            (2, "connect FAILED", "ERR_PHONE_CAMERA"),
            (3, "no frames", "ERR_PHONE_CAMERA"),
            (2, "python3: can't open file '.local/share/vyn-phone/hybcam-1.py': [Errno 2] No such file or directory", "ERR_PHONE_HELPER_MISSING"),
            (1, "Traceback ...", "ERR_PHONE_BACKEND"),
        ] {
            let t = FakeTransport::new();
            t.push_run(Ok(Output { code, stdout: vec![], stderr: stderr.into() }));
            let e = take(&t, &cfg(dir.path()), &reg, &params()).await.unwrap_err();
            assert!(e.to_string().starts_with(want), "{code} {stderr} -> {e}");
        }
    }

    #[tokio::test]
    async fn two_photos_at_once_are_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let t = FakeTransport::new().with_delay(Duration::from_millis(60));
        t.push_run(FakeTransport::ok(encode_frame(&jpeg())));
        t.push_run(FakeTransport::ok(encode_frame(&jpeg())));
        let reg = Arc::new(StreamRegistry::new());
        let c = cfg(dir.path());
        let p = params();
        let (a, b) = tokio::join!(take(&t, &c, &reg, &p), take(&t, &c, &reg, &p));
        a.unwrap();
        b.unwrap();
        assert_eq!(t.max_active.load(Ordering::SeqCst), 1, "the helper must never run twice at once");
    }

    #[tokio::test]
    async fn photo_during_a_stream_copies_the_latest_frame_or_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let t = FakeTransport::new().with_stream(encode_frame(&jpeg()), true);
        let reg = StreamRegistry::new();
        let sp = crate::params::StreamParams::parse(&json!({})).unwrap();
        reg.start(&t, &c, &sp).await.unwrap();
        for _ in 0..100 {
            if reg.status().await["frames"].as_u64().unwrap_or(0) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let v = take(&t, &c, &reg, &params()).await.unwrap();
        assert_eq!(v["source"], json!("stream"));
        assert_eq!(std::fs::read(v["path"].as_str().unwrap()).unwrap(), jpeg());

        let front = PhotoParams::parse(&json!({"camera":"front"})).unwrap();
        assert!(matches!(take(&t, &c, &reg, &front).await, Err(PhoneError::Busy(_))));
        reg.stop(None).await.unwrap();
    }
}
