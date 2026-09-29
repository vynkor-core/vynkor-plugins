# `phone` plugin (v0.1: transport + camera) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `plugins/phone` v0.1.0: an ssh-or-local transport to the Mi 6 and the actions `phone_status`, `phone_setup`, `phone_photo`, `phone_stream_start`, `phone_stream_stop`, `phone_stream_status`.

**Architecture:** A `Transport` trait (`SshTransport` / `LocalTransport` / `FakeTransport`) runs structured `RemoteCmd`s; the only thing deployed on the phone is one hashed helper script (`hybcam-<sha8>.py`, libhybris camera via ctypes) that emits length-prefixed JPEG frames. The plugin turns frames into files (`photo-*.jpg`, atomically replaced `latest.jpg`, optional `record-*.mjpg`). Concurrent SDK loop like `capture`.

**Tech Stack:** Rust 2021, `vynkor-sdk 0.0.3`, tokio, async-trait, thiserror, sha2, serde_json; Python 3 helper (ctypes, cv2 from `~/pylibs` on the phone).

**Spec:** `docs/superpowers/specs/2026-09-29-phone-plugin-design.md`

## How to execute this plan (code extraction)

Every source file below is in a fenced block whose info string carries `file=<repo-relative path>`. The executor extracts them with the script in Task 0, so the plan and the code cannot drift. A task's steps are: extract that task's files, run its tests, commit. Tests live inside the module files (`#[cfg(test)]`) like the rest of this repo.

## Global Constraints

- Plugin id `phone`, crate `phone-plugin`, lib `phone_plugin`, bin `phone`, version `0.1.0`, `vynkor-sdk = "0.0.3"` (repo pin; do not bump).
- `plugin.json` declares **only implemented actions**; every action has `risk` and a non-empty `description`.
- Permissions v0.1: `PERMISSION_NETWORK` (status, setup), `PERMISSION_SCREEN` (photo, stream*). Camera actions: risk `high` + `requires_confirmation: true` (`phone_stream_stop`: medium, `phone_stream_status`: low).
- Env vars `PHONE_PLUGIN_*`: `TRANSPORT` (`ssh`|`local`, default `ssh`), `SSH_HOST` (default `mi6`), `SSH_MUX` (default on, `0` disables), `REMOTE_UID` (default `32011`), `REMOTE_DIR` (default `.local/share/vyn-phone`, relative to the phone's HOME), `DIR` (default `~/.local/share/vyn/phone/`).
- No caller-supplied text reaches a command line: only fixed strings, validated enums, bounded integers.
- ssh: `BatchMode=yes`, key auth only, no `StrictHostKeyChecking` override; host validated `[A-Za-z0-9._-]{1,253}`, never starts with `-`.
- Error codes: `ERR_PHONE_BAD_PARAMS`, `ERR_PHONE_UNREACHABLE`, `ERR_PHONE_BUSY`, `ERR_PHONE_HELPER_MISSING`, `ERR_PHONE_CAMERA`, `ERR_PHONE_BACKEND`.
- Sizes: `1920x1080, 1280x720, 800x600, 640x480, 320x240`; default 1280x720. `fps` 1–30 (default 15), `quality` 30–95 (default 80), `max_duration_ms` default 300000 cap 1800000.
- Timeouts: photo 15 s, status 5 s, setup 15 s. Frame cap 8 MiB.
- Storage `PHONE_PLUGIN_DIR` mode 0700; files `photo-<ms>.jpg`, `latest.jpg`, `record-<ms>.mjpg`.
- Sandbox: `sandbox: false` on the phone (no Landlock on 4.4.153-Halium).
- Build with `cargo <cmd> --manifest-path plugins/phone/Cargo.toml` from the repo root.

## Review Focus

1. Camera held by another process (`lomiri-camera-app`) → the helper prints `connect FAILED`; caller must get `ERR_PHONE_CAMERA` with a bounded stderr tail, not a generic backend error (tested in `error.rs`).
2. ssh dropping mid-stream (EOF on stdout) → `phone_stream_status` reports inactive and `phone_stream_stop` afterwards is a clean `{stopped:false}` / stops idempotently (tested in `stream.rs`).
3. Hostile or malformed params (unknown keys, wrong JSON types, `width` without `height`, sizes off the table, `stream_id` that is not the active one) → `ERR_PHONE_BAD_PARAMS`, never a panic or a partial start (tested in `params.rs`, `stream.rs`).
4. Garbage / truncated / oversized frame from the helper (partial header, zero length, length over the cap, non-JPEG bytes) → an error and **no** file written (tested in `framing.rs`, `photo.rs`).
5. Two `phone_photo` calls at once → serialized on the plugin side (the HAL allows one client), not a second helper fighting for the camera (tested in `photo.rs`).
6. Hostile shell metacharacters in any token rendered for the remote shell (quotes, `;`, `$()`, backticks, newlines, leading `-`) → executed as literal data (tested in `remote.rs` through a real `sh`).

---

## Task 0: Branch, workspace registration, extraction tool

**Files:**
- Modify: `Cargo.toml` (root, currently untracked in the working tree) — add `"plugins/phone"` to `members`
- Create (scratch, not committed): `$SCRATCH/extract_plan.py`

- [ ] **Step 1: Create the branch from `develop`**

```bash
cd /home/behzod/projects/vynkor-core/vynkor-plugins
git switch -c feat/phone-plugin
git status --short
```

Expected: on `feat/phone-plugin`; the pre-existing uncommitted files (`Cargo.toml`, `Cargo.lock`, `scripts/package.sh`, `plugins/_shared/plugin-manifest/Cargo.toml`, an older plan doc) stay uncommitted and are **not** added by any commit in this plan.

- [ ] **Step 2: Write the extractor**

```python
#!/usr/bin/env python3
"""extract_plan.py PLAN.md [--only PATH_PREFIX ...]: write every ```lang file=PATH fence to PATH."""
import re, sys, pathlib
plan = pathlib.Path(sys.argv[1]).read_text()
only = [a for a in sys.argv[2:] if not a.startswith("--")]
pat = re.compile(r"^```[a-zA-Z0-9_+-]* file=(\S+)\n(.*?)^```$", re.S | re.M)
for m in pat.finditer(plan):
    path, body = m.group(1), m.group(2)
    if only and not any(path.startswith(o) for o in only):
        continue
    p = pathlib.Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(body)
    print("wrote", path)
```

- [ ] **Step 3: Register the crate in the (untracked) root workspace**

Add `"plugins/phone",` to `members` in `Cargo.toml` (alphabetical, after `"plugins/notify",`). This file is not part of any commit here; standalone builds keep working through `--manifest-path` because every plugin dir also carries its own `Cargo.lock`.

- [ ] **Step 4: Commit the spec and plan only**

```bash
git add docs/superpowers/specs/2026-09-29-phone-plugin-design.md docs/superpowers/plans/2026-09-29-phone-plugin.md
git commit -m "docs(phone): design spec and implementation plan"
```

---

## Task 1: Crate scaffold, errors, config, params

**Files:**
- Create: `plugins/phone/Cargo.toml`, `plugins/phone/src/lib.rs`, `plugins/phone/src/error.rs`, `plugins/phone/src/config.rs`, `plugins/phone/src/params.rs`, `plugins/phone/src/storage.rs`
- Create (one-line stub, replaced in Task 6): `plugins/phone/src/main.rs`

**Interfaces:**
- Produces: `PhoneError` (+ `classify`, `tail`), `Config`/`TransportKind` (`from_env`, `from_lookup`, `ensure_dir`), `Camera`, `Af`, `PhotoParams::parse`, `StreamParams::parse`, `parse_stream_id`, `storage::{unix_millis, write_atomic}`.

- [ ] **Step 1: Extract the files**

```toml file=plugins/phone/Cargo.toml
[package]
name = "phone-plugin"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
name = "phone_plugin"
path = "src/lib.rs"

[[bin]]
name = "phone"
path = "src/main.rs"

[dependencies]
vynkor-sdk = "0.0.3"
vynkor-plugin-manifest = { path = "../_shared/plugin-manifest" }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "process", "time", "io-util", "sync", "fs"] }
serde_json = "1"
async-trait = "0.1"
thiserror = "1"
sha2 = "0.10"

[dev-dependencies]
tempfile = "3"
```

```rust file=plugins/phone/src/lib.rs
//! `phone` plugin — camera (and later audio/hardware) of a phone reached over
//! ssh or run locally. Every remote action is a structured [`remote::RemoteCmd`]
//! executed by a [`transport::Transport`]; nothing caller-supplied ever reaches
//! a command line. See README.md and the design spec.

pub mod config;
pub mod error;
pub mod framing;
pub mod helper;
pub mod params;
pub mod photo;
pub mod remote;
pub mod status;
pub mod storage;
pub mod stream;
pub mod transport;
```

```rust file=plugins/phone/src/error.rs
//! Typed error taxonomy. Every error carries a stable `ERR_PHONE_*` prefix so
//! callers can branch on the code without parsing prose (same convention as
//! `capture`'s `ERR_CAPTURE_*`).

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PhoneError {
    #[error("ERR_PHONE_BAD_PARAMS: {0}")]
    BadParams(String),
    #[error("ERR_PHONE_UNREACHABLE: {0}")]
    Unreachable(String),
    #[error("ERR_PHONE_BUSY: {0}")]
    Busy(String),
    #[error("ERR_PHONE_HELPER_MISSING: run phone_setup ({0})")]
    HelperMissing(String),
    #[error("ERR_PHONE_CAMERA: {0}")]
    Camera(String),
    #[error("ERR_PHONE_BACKEND: {0}")]
    Backend(String),
}

/// Last `max` characters of `s`, trimmed. Bounds how much remote stderr can
/// leak into an error message.
pub fn tail(s: &str, max: usize) -> String {
    let t = s.trim();
    let n = t.chars().count();
    if n <= max {
        t.to_string()
    } else {
        t.chars().skip(n - max).collect()
    }
}

/// Map a finished remote command (exit code + stderr) to the error taxonomy.
pub fn classify(code: i32, stderr: &str) -> PhoneError {
    let t = tail(stderr, 400);
    if code == 255 {
        return PhoneError::Unreachable(t);
    }
    if stderr.contains("No such file or directory") && stderr.contains("hybcam") {
        return PhoneError::HelperMissing(t);
    }
    if stderr.contains("connect FAILED") || stderr.contains("no frames") {
        return PhoneError::Camera(t);
    }
    PhoneError::Backend(format!("exit {code}: {t}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_exit_255_is_unreachable() {
        assert!(matches!(classify(255, "ssh: connect to host mi6: No route"), PhoneError::Unreachable(_)));
    }

    #[test]
    fn missing_helper_is_named_and_actionable() {
        let e = classify(2, "python3: can't open file 'x/hybcam-ab12cd34.py': [Errno 2] No such file or directory");
        assert!(matches!(e, PhoneError::HelperMissing(_)));
        assert!(e.to_string().contains("run phone_setup"));
    }

    #[test]
    fn camera_held_by_another_process_is_a_camera_error() {
        let e = classify(2, "connect FAILED");
        assert!(matches!(e, PhoneError::Camera(_)));
        assert!(e.to_string().starts_with("ERR_PHONE_CAMERA"));
    }

    #[test]
    fn no_frames_is_a_camera_error() {
        assert!(matches!(classify(3, "no frames"), PhoneError::Camera(_)));
    }

    #[test]
    fn anything_else_is_backend_with_code() {
        let e = classify(1, "boom");
        assert_eq!(e, PhoneError::Backend("exit 1: boom".into()));
    }

    #[test]
    fn stderr_tail_is_bounded() {
        let long = "x".repeat(10_000);
        let e = classify(1, &long);
        assert!(e.to_string().len() < 500, "unbounded stderr leaked: {}", e.to_string().len());
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("  ab  ", 10), "ab");
    }
}
```

```rust file=plugins/phone/src/config.rs
//! Environment configuration (`PHONE_PLUGIN_*`). `from_lookup` takes a lookup
//! closure so tests never mutate the process environment.

use std::path::PathBuf;

use crate::error::PhoneError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Ssh,
    Local,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub transport: TransportKind,
    pub ssh_host: String,
    /// ssh ControlMaster multiplexing (default on; `PHONE_PLUGIN_SSH_MUX=0` disables).
    pub ssh_mux: bool,
    pub remote_uid: u32,
    /// Helper directory, relative to the phone's HOME.
    pub remote_dir: String,
    /// Local data dir: photos, `latest.jpg`, records, the ssh control socket.
    pub dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self, PhoneError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, PhoneError> {
        let val = |k: &str| get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        let transport = match val("PHONE_PLUGIN_TRANSPORT").as_deref() {
            None | Some("ssh") => TransportKind::Ssh,
            Some("local") => TransportKind::Local,
            Some(other) => {
                return Err(PhoneError::BadParams(format!(
                    "PHONE_PLUGIN_TRANSPORT must be 'ssh' or 'local', got '{other}'"
                )))
            }
        };
        let ssh_host = val("PHONE_PLUGIN_SSH_HOST").unwrap_or_else(|| "mi6".to_string());
        validate_host(&ssh_host)?;
        let ssh_mux = val("PHONE_PLUGIN_SSH_MUX").as_deref() != Some("0");
        let remote_uid = match val("PHONE_PLUGIN_REMOTE_UID") {
            None => 32011,
            Some(s) => s
                .parse::<u32>()
                .map_err(|_| PhoneError::BadParams(format!("PHONE_PLUGIN_REMOTE_UID not a uid: '{s}'")))?,
        };
        let remote_dir = val("PHONE_PLUGIN_REMOTE_DIR").unwrap_or_else(|| ".local/share/vyn-phone".to_string());
        validate_remote_dir(&remote_dir)?;
        let dir = match val("PHONE_PLUGIN_DIR") {
            Some(d) => PathBuf::from(d),
            None => {
                let home = get("HOME").unwrap_or_else(|| "/tmp".to_string());
                PathBuf::from(home).join(".local/share/vyn/phone")
            }
        };
        Ok(Self { transport, ssh_host, ssh_mux, remote_uid, remote_dir, dir })
    }

    /// Create the data dir (mode 0700: it holds camera frames). Idempotent.
    pub fn ensure_dir(&self) -> Result<(), PhoneError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| PhoneError::Backend(format!("create {}: {e}", self.dir.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
}

pub fn validate_host(h: &str) -> Result<(), PhoneError> {
    let ok = !h.is_empty()
        && h.len() <= 253
        && !h.starts_with('-')
        && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(PhoneError::BadParams(format!("PHONE_PLUGIN_SSH_HOST invalid: '{h}'")))
    }
}

/// Relative path, components `[A-Za-z0-9._-]+`, no `..`, no leading `-`.
pub fn validate_remote_dir(d: &str) -> Result<(), PhoneError> {
    let bad = |why: &str| PhoneError::BadParams(format!("PHONE_PLUGIN_REMOTE_DIR '{d}': {why}"));
    if d.is_empty() || d.starts_with('/') {
        return Err(bad("must be a non-empty path relative to the phone's HOME"));
    }
    for comp in d.split('/') {
        if comp.is_empty() || comp == ".." || comp == "." || comp.starts_with('-') {
            return Err(bad("empty, '.', '..' or '-'-leading component"));
        }
        if !comp.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
            return Err(bad("only [A-Za-z0-9._-] allowed in components"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, PhoneError> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(move |k| m.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = cfg(&[("HOME", "/home/u")]).unwrap();
        assert_eq!(c.transport, TransportKind::Ssh);
        assert_eq!(c.ssh_host, "mi6");
        assert!(c.ssh_mux);
        assert_eq!(c.remote_uid, 32011);
        assert_eq!(c.remote_dir, ".local/share/vyn-phone");
        assert_eq!(c.dir, PathBuf::from("/home/u/.local/share/vyn/phone"));
    }

    #[test]
    fn overrides() {
        let c = cfg(&[
            ("PHONE_PLUGIN_TRANSPORT", "local"),
            ("PHONE_PLUGIN_SSH_HOST", "phone.lan"),
            ("PHONE_PLUGIN_SSH_MUX", "0"),
            ("PHONE_PLUGIN_REMOTE_UID", "1000"),
            ("PHONE_PLUGIN_REMOTE_DIR", "bin/vp"),
            ("PHONE_PLUGIN_DIR", "/data/p"),
        ])
        .unwrap();
        assert_eq!(c.transport, TransportKind::Local);
        assert_eq!(c.ssh_host, "phone.lan");
        assert!(!c.ssh_mux);
        assert_eq!(c.remote_uid, 1000);
        assert_eq!(c.remote_dir, "bin/vp");
        assert_eq!(c.dir, PathBuf::from("/data/p"));
    }

    #[test]
    fn rejects_bad_transport_host_uid_and_dir() {
        assert!(cfg(&[("PHONE_PLUGIN_TRANSPORT", "telnet")]).is_err());
        for h in ["-oProxyCommand=x", "a b", "a;b", "a$(x)", "h/../x"] {
            assert!(cfg(&[("PHONE_PLUGIN_SSH_HOST", h)]).is_err(), "host {h:?} must be rejected");
        }
        // an empty value means "unset" -> the default host, not an error
        assert_eq!(cfg(&[("PHONE_PLUGIN_SSH_HOST", "  ")]).unwrap().ssh_host, "mi6");
        assert!(cfg(&[("PHONE_PLUGIN_REMOTE_UID", "-1")]).is_err());
        assert!(cfg(&[("PHONE_PLUGIN_REMOTE_UID", "abc")]).is_err());
        for d in ["/abs", "../up", "a/../b", "a//b", "a b", "a;b", "-x", "a/-x", "."] {
            assert!(cfg(&[("PHONE_PLUGIN_REMOTE_DIR", d)]).is_err(), "dir {d:?} must be rejected");
        }
    }

    #[test]
    fn ensure_dir_creates_private_dir() {
        let t = tempfile::tempdir().unwrap();
        let c = cfg(&[("PHONE_PLUGIN_DIR", t.path().join("x/y").to_str().unwrap())]).unwrap();
        c.ensure_dir().unwrap();
        c.ensure_dir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&c.dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }
}
```

```rust file=plugins/phone/src/params.rs
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
```

```rust file=plugins/phone/src/storage.rs
//! Local artifact storage helpers.

use std::path::{Path, PathBuf};

use crate::error::PhoneError;

pub fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis()
}

/// Write `bytes` to `dir/name` atomically (temp file in the same dir, then
/// `rename`), so a reader of `latest.jpg` never sees a torn frame.
pub async fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, PhoneError> {
    let final_path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|e| PhoneError::Backend(format!("write {}: {e}", tmp.display())))?;
    tokio::fs::rename(&tmp, &final_path)
        .await
        .map_err(|e| PhoneError::Backend(format!("rename to {}: {e}", final_path.display())))?;
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn atomic_write_replaces_and_leaves_no_temp() {
        let t = tempfile::tempdir().unwrap();
        write_atomic(t.path(), "latest.jpg", b"one").await.unwrap();
        let p = write_atomic(t.path(), "latest.jpg", b"two-two").await.unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two-two");
        let names: Vec<_> = std::fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["latest.jpg".to_string()], "temp file left behind: {names:?}");
    }
}
```

`main.rs` is a one-line stub until Task 6 (written by the command in Step 2, not extracted, because Task 6 owns the real `main.rs`). The other modules referenced from `lib.rs` are created in Tasks 2–6, so extract **only Task 1's files** and temporarily comment out the not-yet-existing `pub mod` lines — see Step 2.

- [ ] **Step 2: Extract, trim `lib.rs` to the modules that exist, run tests**

```bash
python3 $SCRATCH/extract_plan.py docs/superpowers/plans/2026-09-29-phone-plugin.md plugins/phone/Cargo.toml plugins/phone/src/lib.rs plugins/phone/src/error.rs plugins/phone/src/config.rs plugins/phone/src/params.rs plugins/phone/src/storage.rs
echo 'fn main() {}' > plugins/phone/src/main.rs
# keep only modules that exist so far
python3 - <<'EOF'
import re,pathlib
p=pathlib.Path("plugins/phone/src/lib.rs"); s=p.read_text()
for m in ["framing","helper","photo","remote","status","stream","transport"]:
    s=s.replace(f"pub mod {m};\n", f"// pub mod {m};\n")
p.write_text(s)
EOF
cargo test --manifest-path plugins/phone/Cargo.toml
```

Expected: compiles, `error`, `config`, `params`, `storage` tests pass. (Later tasks restore each `pub mod` line as their file lands: re-extract `lib.rs` at the end of each task and re-comment the modules still missing.)

- [ ] **Step 3: Commit**

```bash
git add plugins/phone
git commit -m "feat(phone): crate scaffold, errors, config, params, storage"
```

---

## Task 2: RemoteCmd and shell quoting

**Files:** Create `plugins/phone/src/remote.rs`

**Interfaces:**
- Produces: `RemoteCmd { env: Vec<(String,String)>, program: String, args: Vec<String>, in_home: bool }` with `RemoteCmd::new(program: &str, args: Vec<String>) -> Self`, `.env(k,v) -> Self`, `.in_home() -> Self`, `.to_shell() -> Result<String, PhoneError>`; `shell_quote(&str) -> String`; `camera_env(uid: u32) -> Vec<(String,String)>`.

- [ ] **Step 1: Extract the file**

```rust file=plugins/phone/src/remote.rs
//! A remote command as *data* (program + argv + env), never a shell string.
//! Only [`RemoteCmd::to_shell`] renders it for a remote shell, quoting every
//! token, so no caller-supplied text can change the command's structure.

use crate::error::PhoneError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCmd {
    pub env: Vec<(String, String)>,
    pub program: String,
    pub args: Vec<String>,
    /// Run with the phone user's HOME as the working directory (helper paths
    /// are HOME-relative).
    pub in_home: bool,
}

/// POSIX single-quote quoting: `'` becomes `'\''`. The result is one word.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn valid_env_key(k: &str) -> bool {
    let mut cs = k.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

impl RemoteCmd {
    pub fn new(program: &str, args: Vec<String>) -> Self {
        Self { env: Vec::new(), program: program.to_string(), args, in_home: false }
    }

    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.to_string(), v.to_string()));
        self
    }

    pub fn envs(mut self, kv: Vec<(String, String)>) -> Self {
        self.env.extend(kv);
        self
    }

    pub fn in_home(mut self) -> Self {
        self.in_home = true;
        self
    }

    /// Render for a POSIX remote shell: `[cd "$HOME" && ]K='v' … 'prog' 'a' 'b'`.
    pub fn to_shell(&self) -> Result<String, PhoneError> {
        let has_nul = |s: &str| s.contains('\0');
        if has_nul(&self.program) || self.args.iter().any(|a| has_nul(a)) || self.env.iter().any(|(_, v)| has_nul(v)) {
            return Err(PhoneError::BadParams("NUL byte in remote command".into()));
        }
        let mut out = String::new();
        if self.in_home {
            out.push_str("cd \"$HOME\" && ");
        }
        for (k, v) in &self.env {
            if !valid_env_key(k) {
                return Err(PhoneError::BadParams(format!("invalid env name '{k}'")));
            }
            out.push_str(k);
            out.push('=');
            out.push_str(&shell_quote(v));
            out.push(' ');
        }
        out.push_str(&shell_quote(&self.program));
        for a in &self.args {
            out.push(' ');
            out.push_str(&shell_quote(a));
        }
        Ok(out)
    }
}

/// Environment the camera helper needs on the phone (same as `qmlscene`).
pub fn camera_env(uid: u32) -> Vec<(String, String)> {
    vec![
        ("XDG_RUNTIME_DIR".to_string(), format!("/run/user/{uid}")),
        ("MIR_SOCKET".to_string(), format!("/run/user/{uid}/mir_socket_trusted")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn renders_a_plain_command() {
        let c = RemoteCmd::new("python3", vec!["a b".into(), "-W".into(), "640".into()]).env("K", "v").in_home();
        assert_eq!(c.to_shell().unwrap(), "cd \"$HOME\" && K='v' 'python3' 'a b' '-W' '640'");
    }

    #[test]
    fn quote_handles_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn hostile_tokens_survive_a_real_shell_as_literal_data() {
        let hostile = [
            "plain",
            "with space",
            "quote'inside",
            "dq\"inside",
            "semi;colon",
            "amp&&echo pwned",
            "$(echo pwned)",
            "`echo pwned`",
            "back\\slash",
            "new\nline",
            "-leading-dash",
            "*glob*",
            "$HOME",
            "a|b>c<d",
            "",
        ];
        for h in hostile {
            let cmd = RemoteCmd::new("printf", vec!["%s".into(), h.to_string()]);
            let out = Command::new("sh").arg("-c").arg(cmd.to_shell().unwrap()).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), h, "token {h:?} was interpreted");
            assert!(out.status.success());
        }
    }

    #[test]
    fn hostile_env_values_survive_a_real_shell() {
        let cmd = RemoteCmd::new("sh", vec!["-c".into(), "printf %s \"$V\"".into()]).env("V", "a'b;$(echo pwned) c");
        let out = Command::new("sh").arg("-c").arg(cmd.to_shell().unwrap()).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a'b;$(echo pwned) c");
    }

    #[test]
    fn invalid_env_names_and_nul_are_rejected() {
        for k in ["lower", "1X", "A-B", "A B", "A=B", "", "A;B"] {
            let c = RemoteCmd::new("true", vec![]).env(k, "v");
            assert!(c.to_shell().is_err(), "env name {k:?} must be rejected");
        }
        assert!(RemoteCmd::new("t\0rue", vec![]).to_shell().is_err());
        assert!(RemoteCmd::new("true", vec!["a\0b".into()]).to_shell().is_err());
    }

    #[test]
    fn in_home_changes_directory_first() {
        let cmd = RemoteCmd::new("pwd", vec![]).in_home();
        let out = Command::new("sh").arg("-c").arg(cmd.to_shell().unwrap()).env("HOME", "/tmp").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "/tmp");
    }

    #[test]
    fn camera_env_uses_the_uid() {
        let e = camera_env(32011);
        assert_eq!(e[0], ("XDG_RUNTIME_DIR".into(), "/run/user/32011".into()));
        assert_eq!(e[1], ("MIR_SOCKET".into(), "/run/user/32011/mir_socket_trusted".into()));
    }
}
```

- [ ] **Step 2: Enable the module and test**

```bash
python3 $SCRATCH/extract_plan.py docs/superpowers/plans/2026-09-29-phone-plugin.md plugins/phone/src/remote.rs
sed -i 's|^// pub mod remote;|pub mod remote;|' plugins/phone/src/lib.rs
cargo test --manifest-path plugins/phone/Cargo.toml remote::
```

Expected: 7 tests pass (including the real-`sh` hostile-token test).

- [ ] **Step 3: Commit** — `git add plugins/phone && git commit -m "feat(phone): RemoteCmd with quoted shell rendering"`

---

## Task 3: Transport (ssh, local, fake) and frame reader

**Files:** Create `plugins/phone/src/transport.rs`, `plugins/phone/src/framing.rs`

**Interfaces:**
- Consumes: `RemoteCmd`, `PhoneError`, `Config`.
- Produces:
  - `Output { code: i32, stdout: Vec<u8>, stderr: String }`
  - `trait Control: Send { async fn kill(&mut self); async fn wait(&mut self) -> Option<i32>; fn stderr_tail(&self) -> String; }`
  - `Spawned { stdout: Box<dyn AsyncRead + Send + Unpin>, control: Box<dyn Control> }`
  - `trait Transport: Send + Sync { async fn run(&self, &RemoteCmd, Option<&[u8]>, Duration) -> Result<Output, PhoneError>; async fn spawn(&self, &RemoteCmd) -> Result<Spawned, PhoneError>; fn describe(&self) -> String; }`
  - `LocalTransport::new()`, `SshTransport::new(host: &str, mux: bool, control_dir: &Path)`, `ssh_args(...)`, `fake::FakeTransport`
  - `framing::{read_frame(&mut R) -> Result<Option<Vec<u8>>, PhoneError>, encode_frame(&[u8]) -> Vec<u8>, is_jpeg(&[u8]) -> bool, MAX_FRAME}`

- [ ] **Step 1: Extract the files**

```rust file=plugins/phone/src/framing.rs
//! Length-prefixed frame reader: the helper writes `u32 big-endian length`
//! followed by that many JPEG bytes, per frame.

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::PhoneError;

/// Hard cap on one frame. A 1080p JPEG is ~200 KB; anything near this is garbage.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

pub fn encode_frame(bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + bytes.len());
    v.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    v.extend_from_slice(bytes);
    v
}

/// SOI at the start and EOI at the end. Cheap sanity check; not a decoder.
pub fn is_jpeg(b: &[u8]) -> bool {
    b.len() >= 4 && b[0] == 0xFF && b[1] == 0xD8 && b[b.len() - 2] == 0xFF && b[b.len() - 1] == 0xD9
}

/// Next frame, `Ok(None)` on a clean EOF at a frame boundary. A partial
/// header, a zero or over-cap length, or a short body is an error.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, PhoneError> {
    let mut hdr = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = r
            .read(&mut hdr[got..])
            .await
            .map_err(|e| PhoneError::Camera(format!("read frame header: {e}")))?;
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(PhoneError::Camera("truncated frame header".into()))
            };
        }
        got += n;
    }
    let len = u32::from_be_bytes(hdr) as usize;
    if len == 0 {
        return Err(PhoneError::Camera("zero-length frame".into()));
    }
    if len > MAX_FRAME {
        return Err(PhoneError::Camera(format!("frame length {len} exceeds cap {MAX_FRAME}")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .await
        .map_err(|e| PhoneError::Camera(format!("truncated frame body: {e}")))?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_consecutive_frames_then_clean_eof() {
        let mut data = encode_frame(&[0xFF, 0xD8, 1, 0xFF, 0xD9]);
        data.extend(encode_frame(&[9, 9]));
        let mut r = &data[..];
        assert_eq!(read_frame(&mut r).await.unwrap().unwrap(), vec![0xFF, 0xD8, 1, 0xFF, 0xD9]);
        assert_eq!(read_frame(&mut r).await.unwrap().unwrap(), vec![9, 9]);
        assert_eq!(read_frame(&mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn reassembles_a_frame_split_across_reads() {
        let data = encode_frame(&vec![7u8; 5000]);
        let (mut w, mut r) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for chunk in data.chunks(3) {
                w.write_all(chunk).await.unwrap();
            }
        });
        let f = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(f.len(), 5000);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_zero_oversized_and_truncated() {
        let mut zero = &[0u8, 0, 0, 0][..];
        assert!(matches!(read_frame(&mut zero).await, Err(PhoneError::Camera(_))));

        let huge = ((MAX_FRAME + 1) as u32).to_be_bytes();
        let mut r = &huge[..];
        assert!(matches!(read_frame(&mut r).await, Err(PhoneError::Camera(_))));

        let mut half_hdr = &[0u8, 0][..];
        assert!(matches!(read_frame(&mut half_hdr).await, Err(PhoneError::Camera(_))));

        let mut short = &[0u8, 0, 0, 10, 1, 2, 3][..];
        assert!(matches!(read_frame(&mut short).await, Err(PhoneError::Camera(_))));
    }

    #[test]
    fn jpeg_check() {
        assert!(is_jpeg(&[0xFF, 0xD8, 0, 0xFF, 0xD9]));
        assert!(!is_jpeg(&[0xFF, 0xD8, 0, 0, 0]));
        assert!(!is_jpeg(b"GIF89a"));
        assert!(!is_jpeg(&[]));
        assert!(!is_jpeg(&[0xFF, 0xD8]));
    }
}
```

```rust file=plugins/phone/src/transport.rs
//! Process-execution boundary. `SshTransport` wraps a [`RemoteCmd`] in `ssh`;
//! `LocalTransport` runs it directly (kernel on the phone); `fake::FakeTransport`
//! is the test double. Same shape as `capture`'s `Spawner`, extended with
//! binary stdout, stdin and long-running children.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::config::{Config, TransportKind};
use crate::error::PhoneError;
use crate::remote::RemoteCmd;

#[derive(Debug, Clone)]
pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

#[async_trait]
pub trait Control: Send {
    /// Kill the child and reap it. Idempotent.
    async fn kill(&mut self);
    async fn wait(&mut self) -> Option<i32>;
    /// Last few KiB of the child's stderr.
    fn stderr_tail(&self) -> String;
}

pub struct Spawned {
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    pub control: Box<dyn Control>,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn run(&self, cmd: &RemoteCmd, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, PhoneError>;
    async fn spawn(&self, cmd: &RemoteCmd) -> Result<Spawned, PhoneError>;
    /// Short label for status output: `ssh:mi6` or `local`.
    fn describe(&self) -> String;
}

/// Build the transport the config asks for.
pub fn from_config(cfg: &Config) -> Arc<dyn Transport> {
    match cfg.transport {
        TransportKind::Ssh => Arc::new(SshTransport::new(&cfg.ssh_host, cfg.ssh_mux, &cfg.dir)),
        TransportKind::Local => Arc::new(LocalTransport::new()),
    }
}

/// A resolved host command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

pub struct LocalTransport;

impl LocalTransport {
    pub fn new() -> Self {
        Self
    }

    pub fn exec(cmd: &RemoteCmd) -> Exec {
        Exec {
            program: cmd.program.clone(),
            args: cmd.args.clone(),
            env: cmd.env.clone(),
            cwd: if cmd.in_home { std::env::var_os("HOME").map(PathBuf::from) } else { None },
        }
    }
}

impl Default for LocalTransport {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SshTransport {
    host: String,
    mux: bool,
    control_dir: PathBuf,
}

/// The exact `ssh` argv for one remote command. Key auth only (`BatchMode`),
/// no host-key policy override, optional ControlMaster multiplexing.
pub fn ssh_args(host: &str, mux: bool, control_dir: &Path, remote_shell: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"].iter().map(|s| s.to_string()).collect();
    if mux {
        a.extend(
            [
                "-o".to_string(),
                "ControlMaster=auto".to_string(),
                "-o".to_string(),
                "ControlPersist=60".to_string(),
                "-o".to_string(),
                format!("ControlPath={}/cm-%C", control_dir.display()),
            ]
            .into_iter(),
        );
    }
    a.push(host.to_string());
    a.push("--".to_string());
    a.push(remote_shell.to_string());
    a
}

impl SshTransport {
    pub fn new(host: &str, mux: bool, control_dir: &Path) -> Self {
        Self { host: host.to_string(), mux, control_dir: control_dir.to_path_buf() }
    }

    pub fn exec(&self, cmd: &RemoteCmd) -> Result<Exec, PhoneError> {
        let shell = cmd.to_shell()?;
        Ok(Exec {
            program: "ssh".to_string(),
            args: ssh_args(&self.host, self.mux, &self.control_dir, &shell),
            env: Vec::new(),
            cwd: None,
        })
    }
}

fn command(e: &Exec) -> Command {
    let mut c = Command::new(&e.program);
    c.args(&e.args).envs(e.env.iter().map(|(k, v)| (k, v))).kill_on_drop(true);
    if let Some(d) = &e.cwd {
        c.current_dir(d);
    }
    c
}

fn spawn_err(program: &str, e: std::io::Error) -> PhoneError {
    if e.kind() == std::io::ErrorKind::NotFound {
        PhoneError::Backend(format!("binary '{program}' not found on PATH"))
    } else {
        PhoneError::Backend(format!("spawn '{program}' failed: {e}"))
    }
}

async fn run_exec(e: &Exec, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, PhoneError> {
    let mut c = command(e);
    c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|err| spawn_err(&e.program, err))?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let data = data.to_vec();
        tokio::spawn(async move {
            let _ = pipe.write_all(&data).await;
            let _ = pipe.shutdown().await;
        });
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => Ok(Output {
            code: o.status.code().unwrap_or(-1),
            stdout: o.stdout,
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
        Ok(Err(err)) => Err(PhoneError::Backend(format!("wait '{}' failed: {err}", e.program))),
        Err(_) => Err(PhoneError::Unreachable(format!("timed out after {}s", timeout.as_secs()))),
    }
}

struct ChildControl {
    child: Child,
    stderr: Arc<Mutex<Vec<u8>>>,
}

const STDERR_KEEP: usize = 4096;

#[async_trait]
impl Control for ChildControl {
    async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
    async fn wait(&mut self) -> Option<i32> {
        self.child.wait().await.ok().and_then(|s| s.code())
    }
    fn stderr_tail(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }
}

async fn spawn_exec(e: &Exec) -> Result<Spawned, PhoneError> {
    let mut c = command(e);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|err| spawn_err(&e.program, err))?;
    let stdout = child.stdout.take().ok_or_else(|| PhoneError::Backend("child stdout missing".into()))?;
    let mut stderr = child.stderr.take().ok_or_else(|| PhoneError::Backend("child stderr missing".into()))?;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buf);
    tokio::spawn(async move {
        let mut chunk = [0u8; 512];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut b = sink.lock().unwrap();
                    b.extend_from_slice(&chunk[..n]);
                    if b.len() > STDERR_KEEP {
                        let cut = b.len() - STDERR_KEEP;
                        b.drain(..cut);
                    }
                }
            }
        }
    });
    Ok(Spawned { stdout: Box::new(stdout), control: Box::new(ChildControl { child, stderr: buf }) })
}

#[async_trait]
impl Transport for LocalTransport {
    async fn run(&self, cmd: &RemoteCmd, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, PhoneError> {
        run_exec(&Self::exec(cmd), stdin, timeout).await
    }
    async fn spawn(&self, cmd: &RemoteCmd) -> Result<Spawned, PhoneError> {
        spawn_exec(&Self::exec(cmd)).await
    }
    fn describe(&self) -> String {
        "local".to_string()
    }
}

#[async_trait]
impl Transport for SshTransport {
    async fn run(&self, cmd: &RemoteCmd, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, PhoneError> {
        run_exec(&self.exec(cmd)?, stdin, timeout).await
    }
    async fn spawn(&self, cmd: &RemoteCmd) -> Result<Spawned, PhoneError> {
        spawn_exec(&self.exec(cmd)?).await
    }
    fn describe(&self) -> String {
        format!("ssh:{}", self.host)
    }
}

/// Test double used by unit tests here and the fake-kernel test in `main.rs`.
pub mod fake {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;

    pub struct FakeTransport {
        pub calls: Mutex<Vec<RemoteCmd>>,
        pub stdins: Mutex<Vec<Option<Vec<u8>>>>,
        runs: Mutex<VecDeque<Result<Output, PhoneError>>>,
        stream_bytes: Mutex<Vec<u8>>,
        hold_open: bool,
        pub killed: Arc<AtomicBool>,
        delay: Duration,
        active: AtomicUsize,
        pub max_active: AtomicUsize,
    }

    impl FakeTransport {
        pub fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                stdins: Mutex::new(Vec::new()),
                runs: Mutex::new(VecDeque::new()),
                stream_bytes: Mutex::new(Vec::new()),
                hold_open: false,
                killed: Arc::new(AtomicBool::new(false)),
                delay: Duration::ZERO,
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
            }
        }
        pub fn push_run(&self, r: Result<Output, PhoneError>) {
            self.runs.lock().unwrap().push_back(r);
        }
        pub fn ok(stdout: Vec<u8>) -> Result<Output, PhoneError> {
            Ok(Output { code: 0, stdout, stderr: String::new() })
        }
        /// Bytes the next `spawn` emits on stdout; `hold_open` keeps the pipe
        /// open (a live stream) until `kill`.
        pub fn with_stream(mut self, bytes: Vec<u8>, hold_open: bool) -> Self {
            *self.stream_bytes.lock().unwrap() = bytes;
            self.hold_open = hold_open;
            self
        }
        pub fn with_delay(mut self, d: Duration) -> Self {
            self.delay = d;
            self
        }
    }

    impl Default for FakeTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    struct FakeControl {
        killed: Arc<AtomicBool>,
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl Control for FakeControl {
        async fn kill(&mut self) {
            self.killed.store(true, Ordering::SeqCst);
            self.notify.notify_one();
        }
        async fn wait(&mut self) -> Option<i32> {
            Some(0)
        }
        fn stderr_tail(&self) -> String {
            String::new()
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn run(&self, cmd: &RemoteCmd, stdin: Option<&[u8]>, _t: Duration) -> Result<Output, PhoneError> {
            self.calls.lock().unwrap().push(cmd.clone());
            self.stdins.lock().unwrap().push(stdin.map(|s| s.to_vec()));
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.runs
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(PhoneError::Backend("FakeTransport: no scripted run result".into())))
        }

        async fn spawn(&self, cmd: &RemoteCmd) -> Result<Spawned, PhoneError> {
            use tokio::io::AsyncWriteExt as _;
            self.calls.lock().unwrap().push(cmd.clone());
            let (mut w, r) = tokio::io::duplex(8 * 1024 * 1024);
            let bytes = self.stream_bytes.lock().unwrap().clone();
            let hold = self.hold_open;
            let notify = Arc::new(Notify::new());
            let n2 = Arc::clone(&notify);
            tokio::spawn(async move {
                let _ = w.write_all(&bytes).await;
                if hold {
                    n2.notified().await;
                }
                drop(w);
            });
            Ok(Spawned {
                stdout: Box::new(r),
                control: Box::new(FakeControl { killed: Arc::clone(&self.killed), notify }),
            })
        }

        fn describe(&self) -> String {
            "fake".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_argv_shape_with_and_without_mux() {
        let a = ssh_args("mi6", true, Path::new("/d"), "cd \"$HOME\" && 'x'");
        assert_eq!(&a[0..4], ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]);
        assert!(a.contains(&"ControlPath=/d/cm-%C".to_string()));
        assert!(a.contains(&"ControlPersist=60".to_string()));
        assert_eq!(a[a.len() - 3], "mi6");
        assert_eq!(a[a.len() - 2], "--");
        assert_eq!(a[a.len() - 1], "cd \"$HOME\" && 'x'");
        assert!(!a.iter().any(|s| s.contains("StrictHostKeyChecking")));

        let b = ssh_args("mi6", false, Path::new("/d"), "x");
        assert!(!b.iter().any(|s| s.contains("Control")));
    }

    #[test]
    fn ssh_exec_quotes_the_remote_command() {
        let t = SshTransport::new("mi6", false, Path::new("/d"));
        let e = t.exec(&RemoteCmd::new("python3", vec!["a b; rm -rf /".into()]).in_home()).unwrap();
        assert_eq!(e.program, "ssh");
        assert_eq!(e.args.last().unwrap(), "cd \"$HOME\" && 'python3' 'a b; rm -rf /'");
    }

    #[test]
    fn local_exec_uses_home_as_cwd_only_when_asked() {
        let plain = LocalTransport::exec(&RemoteCmd::new("true", vec![]));
        assert_eq!(plain.cwd, None);
        let home = LocalTransport::exec(&RemoteCmd::new("true", vec![]).in_home());
        assert_eq!(home.cwd, std::env::var_os("HOME").map(PathBuf::from));
    }

    #[tokio::test]
    async fn local_run_captures_binary_stdout_stdin_and_exit_code() {
        let t = LocalTransport::new();
        let o = t
            .run(&RemoteCmd::new("printf", vec!["%s".into(), "hi".into()]), None, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!((o.code, o.stdout.as_slice()), (0, &b"hi"[..]));

        let payload: Vec<u8> = (0u8..=255).collect();
        let o = t.run(&RemoteCmd::new("cat", vec![]), Some(&payload), Duration::from_secs(5)).await.unwrap();
        assert_eq!(o.stdout, payload, "binary stdin/stdout must round-trip");

        let o = t.run(&RemoteCmd::new("sh", vec!["-c".into(), "echo err >&2; exit 7".into()]), None, Duration::from_secs(5)).await.unwrap();
        assert_eq!(o.code, 7);
        assert_eq!(o.stderr.trim(), "err");
    }

    #[tokio::test]
    async fn local_run_times_out_and_missing_binary_is_reported() {
        let t = LocalTransport::new();
        let e = t.run(&RemoteCmd::new("sleep", vec!["5".into()]), None, Duration::from_millis(100)).await.unwrap_err();
        assert!(matches!(e, PhoneError::Unreachable(_)), "{e}");
        let e = t.run(&RemoteCmd::new("definitely-not-a-binary-xyz", vec![]), None, Duration::from_secs(1)).await.unwrap_err();
        assert!(matches!(e, PhoneError::Backend(_)) && e.to_string().contains("not found"), "{e}");
    }

    #[tokio::test]
    async fn local_spawn_streams_stdout_and_kill_is_idempotent() {
        let t = LocalTransport::new();
        let mut s = t
            .spawn(&RemoteCmd::new("sh", vec!["-c".into(), "printf abc; echo oops >&2; sleep 30".into()]))
            .await
            .unwrap();
        let mut buf = [0u8; 3];
        tokio::io::AsyncReadExt::read_exact(&mut s.stdout, &mut buf).await.unwrap();
        assert_eq!(&buf, b"abc");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(s.control.stderr_tail().contains("oops"));
        s.control.kill().await;
        s.control.kill().await;
    }
}
```

- [ ] **Step 2: Extract, enable modules, test**

```bash
python3 $SCRATCH/extract_plan.py docs/superpowers/plans/2026-09-29-phone-plugin.md plugins/phone/src/framing.rs plugins/phone/src/transport.rs
sed -i 's|^// pub mod framing;|pub mod framing;|; s|^// pub mod transport;|pub mod transport;|' plugins/phone/src/lib.rs
cargo test --manifest-path plugins/phone/Cargo.toml transport:: framing::
```

Expected: all pass (framing 4 tests, transport 6 tests; they spawn real `printf`/`cat`/`sh`/`sleep`).

- [ ] **Step 3: Commit** — `git add plugins/phone && git commit -m "feat(phone): transport trait (ssh/local/fake) and length-prefixed frame reader"`

---

## Task 4: Helper script and deployment

**Files:** Create `plugins/phone/helper/hybcam.py`, `plugins/phone/src/helper.rs`

**Interfaces:**
- Consumes: `Transport`, `RemoteCmd`, `Config`.
- Produces: `helper::{SCRIPT, sha8(), file_name(), remote_path(&Config) -> String, HelperState {Ok, Stale, Missing}, check(&dyn Transport, &Config) -> Result<HelperState, PhoneError>, setup(&dyn Transport, &Config) -> Result<SetupResult, PhoneError>, helper_cmd(&Config, args: Vec<String>) -> RemoteCmd}`; `SetupResult { installed: bool, changed: bool, helper_path: String, sha8: String }`.

- [ ] **Step 1: Extract the files**

```python file=plugins/phone/helper/hybcam.py
#!/usr/bin/python3
"""Camera preview frames via the libhybris camera compat layer (libcamera.so.1).
No window, no Mir surface, no shutter click. Deployed by the `phone` plugin's
phone_setup as hybcam-<sha8>.py.

Frames go to --out FILE or stdout ('-'); log/stats go to stderr.
  --fmt raw|y|jpeg      NV21 / luma only / JPEG (cv2 from ~/pylibs)
  --framing none|len32  len32 = 4-byte big-endian length before every frame
  --snap                emit ONE frame after autofocus settled, then exit
Exit codes: 0 ok, 2 connect failed, 3 no frames.
"""
import argparse
import ctypes as C
import os
import signal
import struct
import sys
import time

ap = argparse.ArgumentParser()
ap.add_argument("--cam", default="back", choices=["back", "front"])
ap.add_argument("-W", type=int, default=640)
ap.add_argument("-H", type=int, default=480)
ap.add_argument("--fps", type=int, default=15)
ap.add_argument("--secs", type=float, default=5, help="hard deadline (dead-man switch)")
ap.add_argument("--out", default="-")
ap.add_argument("--fmt", default="raw", choices=["raw", "y", "jpeg"])
ap.add_argument("--framing", default="none", choices=["none", "len32"])
ap.add_argument("--q", type=int, default=70, help="jpeg quality")
ap.add_argument("--af", default="", help="off|video|auto|macro|picture|infinity")
ap.add_argument("--flash", default="", help="0 off, 1 auto, 2 on, 3 torch")
ap.add_argument("--snap", action="store_true")
ap.add_argument("--settle", type=float, default=1.5, help="--snap: seconds after first frame")
ap.add_argument("--dump", action="store_true")
a = ap.parse_args()

if a.fmt == "jpeg":
    sys.path.insert(0, os.path.expanduser("~/pylibs"))
    import numpy as np
    import cv2

W, H = a.W, a.H
STOP = {"flag": False}


def log(*x):
    print(*x, file=sys.stderr, flush=True)


def on_signal(signum, frame):
    STOP["flag"] = True


for s in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
    signal.signal(s, on_signal)

lib = C.CDLL("libcamera.so.1")
VOIDP = C.c_void_p
CB_CTX = C.CFUNCTYPE(None, VOIDP)
CB_ZOOM = C.CFUNCTYPE(None, VOIDP, C.c_int32)
CB_DATA = C.CFUNCTYPE(None, VOIDP, C.c_uint32, VOIDP)
CB_SIZE = C.CFUNCTYPE(None, VOIDP, C.c_int, C.c_int)


class Listener(C.Structure):
    _fields_ = [
        ("on_msg_error_cb", CB_CTX), ("on_msg_shutter_cb", CB_CTX), ("on_msg_focus_cb", CB_CTX),
        ("on_msg_zoom_cb", CB_ZOOM), ("on_data_raw_image_cb", CB_DATA),
        ("on_data_compressed_image_cb", CB_DATA), ("on_preview_texture_needs_update_cb", CB_CTX),
        ("context", VOIDP), ("on_preview_frame_cb", CB_DATA),
    ]


fd = 1 if a.out == "-" else os.open(a.out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC)
st = {"n": 0, "t0": None, "last_raw": None}


def emit(buf):
    if a.framing == "len32":
        buf = struct.pack(">I", len(buf)) + bytes(buf)
    mv = memoryview(buf)
    try:
        while mv:
            w = os.write(fd, mv)
            mv = mv[w:]
    except (BrokenPipeError, OSError):
        STOP["flag"] = True


def encode(raw):
    if a.fmt == "raw":
        return bytes(raw)
    if a.fmt == "y":
        return bytes(raw[: W * H])
    yuv = np.frombuffer(raw, dtype=np.uint8).reshape(H * 3 // 2, W)
    bgr = cv2.cvtColor(yuv, cv2.COLOR_YUV2BGR_NV21)
    ok, enc = cv2.imencode(".jpg", bgr, [cv2.IMWRITE_JPEG_QUALITY, a.q])
    return enc.tobytes()


def on_frame(data, size, ctx):
    if st["t0"] is None:
        st["t0"] = time.time()
    st["n"] += 1
    raw = (C.c_char * size).from_address(data)
    if a.snap:
        st["last_raw"] = bytes(raw)
        return
    emit(encode(raw))


keep = [
    CB_CTX(lambda c: log("ERROR cb")), CB_CTX(lambda c: log("SHUTTER cb")), CB_CTX(lambda c: None),
    CB_ZOOM(lambda c, z: None), CB_DATA(lambda d, s, c: None), CB_DATA(lambda d, s, c: None),
    CB_CTX(lambda c: None), CB_DATA(on_frame),
]
lst = Listener(*keep[:7], None, keep[7])

lib.android_camera_connect_to.restype = VOIDP
lib.android_camera_connect_to.argtypes = [C.c_int, C.POINTER(Listener)]
ctl = lib.android_camera_connect_to(0 if a.cam == "back" else 1, C.byref(lst))
if not ctl:
    log("connect FAILED")
    sys.exit(2)


def release():
    try:
        lib.android_camera_stop_preview.argtypes = [VOIDP]
        lib.android_camera_stop_preview(ctl)
        lib.android_camera_disconnect.argtypes = [VOIDP]
        lib.android_camera_disconnect(ctl)
    except Exception as e:  # never mask the real exit path
        log("release error", e)


try:
    if a.dump:
        lib.android_camera_dump_parameters.argtypes = [VOIDP]
        lib.android_camera_dump_parameters(ctl)

    lib.android_camera_set_preview_size.argtypes = [VOIDP, C.c_int, C.c_int]
    lib.android_camera_set_preview_size(ctl, W, H)
    lib.android_camera_set_preview_format.argtypes = [VOIDP, C.c_int]
    lib.android_camera_set_preview_format(ctl, 1)  # CAMERA_PIXEL_FORMAT_YUV420SP (NV21)
    lib.android_camera_set_preview_fps.argtypes = [VOIDP, C.c_int]
    lib.android_camera_set_preview_fps(ctl, a.fps)

    if a.af:
        modes = {"off": 0, "video": 1, "auto": 2, "macro": 3, "picture": 4, "infinity": 5}
        lib.android_camera_set_auto_focus_mode.argtypes = [VOIDP, C.c_int]
        lib.android_camera_set_auto_focus_mode(ctl, modes[a.af])
    if a.flash:
        lib.android_camera_set_flash_mode.argtypes = [VOIDP, C.c_int]
        lib.android_camera_set_flash_mode(ctl, int(a.flash))

    # Without a preview target Camera2Client parks in WAITING_FOR_PREVIEW_WINDOW and
    # delivers nothing. GLConsumer only remembers the texture id until
    # updateTexImage(), which we never call, so a dummy id needs no GL context.
    lib.android_camera_set_preview_texture.argtypes = [VOIDP, C.c_int]
    lib.android_camera_set_preview_texture(ctl, 1)
    lib.android_camera_set_preview_callback_mode.argtypes = [VOIDP, C.c_int]
    lib.android_camera_set_preview_callback_mode(ctl, 1)
    lib.android_camera_start_preview.argtypes = [VOIDP]
    lib.android_camera_start_preview(ctl)

    if a.af == "auto":
        time.sleep(1)
        lib.android_camera_start_autofocus.argtypes = [VOIDP]
        lib.android_camera_start_autofocus(ctl)

    deadline = time.time() + a.secs
    while not STOP["flag"] and time.time() < deadline:
        if a.snap and st["t0"] is not None and time.time() - st["t0"] >= a.settle and st["last_raw"]:
            emit(encode(st["last_raw"]))
            break
        time.sleep(0.05)
finally:
    release()

if a.snap and st["n"] == 0:
    log("no frames")
    os._exit(3)
if not a.snap:
    log("frames", st["n"])
os._exit(0)
```

```rust file=plugins/phone/src/helper.rs
//! The one artifact deployed on the phone: `hybcam-<sha8>.py` under
//! `<HOME>/<remote_dir>/`. The hash in the file name means a plugin upgrade
//! never runs a stale helper.

use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::PhoneError;
use crate::remote::{camera_env, RemoteCmd};
use crate::transport::Transport;

pub const SCRIPT: &str = include_str!("../helper/hybcam.py");

pub fn sha8() -> String {
    let d = Sha256::digest(SCRIPT.as_bytes());
    d.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

pub fn file_name() -> String {
    format!("hybcam-{}.py", sha8())
}

/// HOME-relative path of the current helper.
pub fn remote_path(cfg: &Config) -> String {
    format!("{}/{}", cfg.remote_dir, file_name())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperState {
    Ok,
    Stale,
    Missing,
}

impl HelperState {
    pub fn as_str(self) -> &'static str {
        match self {
            HelperState::Ok => "ok",
            HelperState::Stale => "stale",
            HelperState::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupResult {
    pub installed: bool,
    pub changed: bool,
    pub helper_path: String,
    pub sha8: String,
}

/// `python3 <helper> <args…>` with the camera environment, run from HOME.
pub fn helper_cmd(cfg: &Config, mut args: Vec<String>) -> RemoteCmd {
    args.insert(0, remote_path(cfg));
    RemoteCmd::new("python3", args).envs(camera_env(cfg.remote_uid)).in_home()
}

pub fn parse_state(listing: &str) -> HelperState {
    let current = file_name();
    let mut any_old = false;
    for line in listing.lines() {
        let l = line.trim();
        if l == current {
            return HelperState::Ok;
        }
        if l.starts_with("hybcam-") && l.ends_with(".py") {
            any_old = true;
        }
    }
    if any_old {
        HelperState::Stale
    } else {
        HelperState::Missing
    }
}

pub async fn check(t: &dyn Transport, cfg: &Config) -> Result<HelperState, PhoneError> {
    // `ls` of a missing dir prints nothing and exits nonzero; that is "missing", not an error.
    let cmd = RemoteCmd::new(
        "sh",
        vec!["-c".into(), "ls -1 \"$1\" 2>/dev/null; true".into(), "sh".into(), cfg.remote_dir.clone()],
    )
    .in_home();
    let o = t.run(&cmd, None, Duration::from_secs(5)).await?;
    if o.code == 255 {
        return Err(crate::error::classify(o.code, &o.stderr));
    }
    Ok(parse_state(&String::from_utf8_lossy(&o.stdout)))
}

pub async fn setup(t: &dyn Transport, cfg: &Config) -> Result<SetupResult, PhoneError> {
    let name = file_name();
    let result = |changed| SetupResult {
        installed: true,
        changed,
        helper_path: remote_path(cfg),
        sha8: sha8(),
    };
    if check(t, cfg).await? == HelperState::Ok {
        return Ok(result(false));
    }
    // atomic: write a dot-temp file, then rename; mkdir -p first
    let script = "mkdir -p \"$1\" && cat > \"$1/.$2.tmp\" && mv \"$1/.$2.tmp\" \"$1/$2\"";
    let cmd = RemoteCmd::new(
        "sh",
        vec!["-c".into(), script.into(), "sh".into(), cfg.remote_dir.clone(), name],
    )
    .in_home();
    let o = t.run(&cmd, Some(SCRIPT.as_bytes()), Duration::from_secs(15)).await?;
    if o.code != 0 {
        return Err(crate::error::classify(o.code, &o.stderr));
    }
    Ok(result(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::fake::FakeTransport;
    use crate::transport::Output;

    fn cfg() -> Config {
        Config::from_lookup(|k| match k {
            "HOME" => Some("/h".into()),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn hash_is_stable_eight_hex_and_in_the_file_name() {
        let s = sha8();
        assert_eq!(s.len(), 8);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(file_name(), format!("hybcam-{s}.py"));
        assert_eq!(remote_path(&cfg()), format!(".local/share/vyn-phone/hybcam-{s}.py"));
    }

    #[test]
    fn parse_state_distinguishes_ok_stale_missing() {
        assert_eq!(parse_state(&format!("x\n{}\n", file_name())), HelperState::Ok);
        assert_eq!(parse_state("hybcam-00000000.py\nother\n"), HelperState::Stale);
        assert_eq!(parse_state(""), HelperState::Missing);
        assert_eq!(parse_state("readme\n"), HelperState::Missing);
    }

    #[test]
    fn helper_cmd_runs_from_home_with_camera_env() {
        let c = helper_cmd(&cfg(), vec!["--cam".into(), "back".into()]);
        assert_eq!(c.program, "python3");
        assert_eq!(c.args[0], remote_path(&cfg()));
        assert_eq!(&c.args[1..], ["--cam", "back"]);
        assert!(c.in_home);
        assert!(c.env.iter().any(|(k, v)| k == "XDG_RUNTIME_DIR" && v == "/run/user/32011"));
        assert!(c.env.iter().any(|(k, _)| k == "MIR_SOCKET"));
    }

    #[tokio::test]
    async fn setup_is_idempotent_when_the_helper_is_already_there() {
        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(format!("{}\n", file_name()).into_bytes()));
        let r = setup(&t, &cfg()).await.unwrap();
        assert!(r.installed && !r.changed);
        assert_eq!(t.calls.lock().unwrap().len(), 1, "no deploy when already current");
    }

    #[tokio::test]
    async fn setup_deploys_the_embedded_script_over_stdin() {
        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(b"hybcam-00000000.py\n".to_vec())); // stale
        t.push_run(FakeTransport::ok(Vec::new())); // deploy
        let r = setup(&t, &cfg()).await.unwrap();
        assert!(r.installed && r.changed);
        let stdins = t.stdins.lock().unwrap();
        assert_eq!(stdins[1].as_deref(), Some(SCRIPT.as_bytes()));
        let calls = t.calls.lock().unwrap();
        assert!(calls[1].args.contains(&file_name()));
    }

    #[tokio::test]
    async fn setup_surfaces_a_failed_deploy_and_unreachable() {
        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(Vec::new()));
        t.push_run(Ok(Output { code: 1, stdout: vec![], stderr: "read-only file system".into() }));
        assert!(matches!(setup(&t, &cfg()).await, Err(PhoneError::Backend(_))));

        let t = FakeTransport::new();
        t.push_run(Ok(Output { code: 255, stdout: vec![], stderr: "no route".into() }));
        assert!(matches!(setup(&t, &cfg()).await, Err(PhoneError::Unreachable(_))));
    }

    #[test]
    fn embedded_helper_is_valid_python() {
        // Skipped silently where python3 is absent (CI without it).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h.py");
        std::fs::write(&p, SCRIPT).unwrap();
        match std::process::Command::new("python3").args(["-m", "py_compile"]).arg(&p).output() {
            Ok(o) => assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr)),
            Err(_) => eprintln!("python3 not found; skipping"),
        }
    }
}
```

- [ ] **Step 2: Extract, enable, test**

```bash
python3 $SCRATCH/extract_plan.py docs/superpowers/plans/2026-09-29-phone-plugin.md plugins/phone/helper/hybcam.py plugins/phone/src/helper.rs
sed -i 's|^// pub mod helper;|pub mod helper;|' plugins/phone/src/lib.rs
cargo test --manifest-path plugins/phone/Cargo.toml helper::
```

Expected: 7 tests pass (the py_compile test runs when `python3` is installed).

- [ ] **Step 3: Commit** — `git add plugins/phone && git commit -m "feat(phone): hybcam helper (len32 frames, --snap, dead-man) and hashed deployment"`

---

## Task 5: Stream registry and photo

**Files:** Create `plugins/phone/src/stream.rs`, `plugins/phone/src/photo.rs`

**Interfaces:**
- Consumes: `Transport`, `Config`, `helper::{helper_cmd, check?}`, `framing`, `params`, `storage`, `PhoneError`, `classify`.
- Produces:
  - `stream::StreamRegistry::new() -> Self`; `async fn start(&self, &dyn Transport, &Config, &StreamParams) -> Result<Value, PhoneError>`; `async fn stop(&self, Option<String>) -> Result<Value, PhoneError>`; `async fn status(&self) -> Value`; `async fn active_camera(&self) -> Option<(Camera, PathBuf)>`; `photo_lock: tokio::sync::Mutex<()>` field access via `registry.camera_guard().await`.
  - `photo::take(&dyn Transport, &Config, &StreamRegistry, &PhotoParams) -> Result<Value, PhoneError>`; `photo::photo_args(&PhotoParams) -> Vec<String>`; `stream::stream_args(&StreamParams) -> Vec<String>`.

- [ ] **Step 1: Extract the files**

```rust file=plugins/phone/src/stream.rs
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
    loop {
        match read_frame(&mut stdout).await {
            Ok(Some(frame)) => {
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
                stats
                    .last_frame_ms
                    .store((started.elapsed().as_millis() as u64).max(1), Ordering::SeqCst);
            }
            Ok(None) | Err(_) => break,
        }
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
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls[0].program, "python3");
        assert!(calls[0].args[0].contains("hybcam-"));
        assert!(calls[0].in_home);
        drop(calls);
        reg.stop(None).await.unwrap();
    }
}
```

```rust file=plugins/phone/src/photo.rs
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
```

- [ ] **Step 2: Extract, enable, test**

```bash
python3 $SCRATCH/extract_plan.py docs/superpowers/plans/2026-09-29-phone-plugin.md plugins/phone/src/stream.rs plugins/phone/src/photo.rs
sed -i 's|^// pub mod stream;|pub mod stream;|; s|^// pub mod photo;|pub mod photo;|' plugins/phone/src/lib.rs
cargo test --manifest-path plugins/phone/Cargo.toml stream:: photo::
```

Expected: 7 stream tests + 6 photo tests pass.

- [ ] **Step 3: Commit** — `git add plugins/phone && git commit -m "feat(phone): stream registry (latest.jpg, record, dead-man) and photo"`

---

## Task 6: Status and the plugin binary (`plugin.json`, `main.rs`, fake-kernel test)

**Files:** Create `plugins/phone/src/status.rs`, `plugins/phone/plugin.json`; replace `plugins/phone/src/main.rs`.

**Interfaces:**
- Consumes: everything above.
- Produces: `status::status(&dyn Transport, &Config, &StreamRegistry) -> Value`; binary `phone`.

- [ ] **Step 1: Extract the files**

```rust file=plugins/phone/src/status.rs
//! `phone_status`: is the phone reachable, is the helper installed, is a
//! stream running. Never fails: an unreachable phone is a *result*.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::PhoneError;
use crate::helper::{self, HelperState};
use crate::stream::StreamRegistry;
use crate::transport::Transport;

pub async fn status(t: &dyn Transport, cfg: &Config, reg: &StreamRegistry) -> Value {
    let started = Instant::now();
    let checked = helper::check(t, cfg).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let stream_active = reg.is_active().await;
    let (reachable, helper_state, error) = match checked {
        Ok(s) => (true, s.as_str(), None),
        Err(PhoneError::Unreachable(m)) => (false, "unknown", Some(PhoneError::Unreachable(m).to_string())),
        Err(e) => (true, "unknown", Some(e.to_string())),
    };
    let mut v = json!({
        "transport": t.describe(),
        "host": cfg.ssh_host,
        "reachable": reachable,
        "helper": helper_state,
        "stream_active": stream_active,
        "latency_ms": latency_ms,
    });
    if let Some(e) = error {
        v["error"] = json!(e);
    }
    let _ = Duration::ZERO;
    let _ = HelperState::Ok;
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::fake::FakeTransport;
    use crate::transport::Output;

    fn cfg() -> Config {
        Config::from_lookup(|_| None).unwrap()
    }

    #[tokio::test]
    async fn reports_ok_missing_and_unreachable_without_erroring() {
        let reg = StreamRegistry::new();

        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(format!("{}\n", helper::file_name()).into_bytes()));
        let v = status(&t, &cfg(), &reg).await;
        assert_eq!(v["reachable"], json!(true));
        assert_eq!(v["helper"], json!("ok"));
        assert_eq!(v["transport"], json!("fake"));
        assert_eq!(v["stream_active"], json!(false));

        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(Vec::new()));
        assert_eq!(status(&t, &cfg(), &reg).await["helper"], json!("missing"));

        let t = FakeTransport::new();
        t.push_run(Ok(Output { code: 255, stdout: vec![], stderr: "no route".into() }));
        let v = status(&t, &cfg(), &reg).await;
        assert_eq!(v["reachable"], json!(false));
        assert!(v["error"].as_str().unwrap().starts_with("ERR_PHONE_UNREACHABLE"));

        let t = FakeTransport::new(); // no scripted result -> backend error, but reachable
        let v = status(&t, &cfg(), &reg).await;
        assert_eq!(v["reachable"], json!(true));
        assert!(v["error"].as_str().unwrap().starts_with("ERR_PHONE_BACKEND"));
    }
}
```

```json file=plugins/phone/plugin.json
{
  "plugin_id": "phone",
  "version": "0.1.0",
  "permissions": ["PERMISSION_NETWORK", "PERMISSION_SCREEN"],
  "kernel_compatibility_range": { "min": "0.1.0", "max": "*" },
  "binary": "phone",
  "events": [],
  "files": ["phone", "plugin.json"],
  "actions": [
    {
      "name": "phone_status",
      "description": "Check the phone: is it reachable (ssh or local), is the camera helper installed on it, and is a camera stream running; call this first when a phone_* action fails.",
      "risk": "low",
      "permission": "PERMISSION_NETWORK",
      "input": { "type": "object", "properties": {}, "additionalProperties": false },
      "output": {
        "type": "object",
        "properties": {
          "transport": { "type": "string", "description": "ssh:<host> or local." },
          "host": { "type": "string" },
          "reachable": { "type": "boolean" },
          "helper": { "type": "string", "enum": ["ok", "stale", "missing", "unknown"] },
          "stream_active": { "type": "boolean" },
          "latency_ms": { "type": "integer" },
          "error": { "type": "string", "description": "Present when the check itself failed." }
        },
        "required": ["transport", "reachable", "helper", "stream_active"]
      }
    },
    {
      "name": "phone_setup",
      "description": "Install or update the small camera helper script in the phone's home directory (idempotent; changes nothing when it is already current). Run once after installing the plugin or after a plugin upgrade.",
      "risk": "medium",
      "permission": "PERMISSION_NETWORK",
      "input": { "type": "object", "properties": {}, "additionalProperties": false },
      "output": {
        "type": "object",
        "properties": {
          "installed": { "type": "boolean" },
          "changed": { "type": "boolean", "description": "False when the helper was already current." },
          "helper_path": { "type": "string", "description": "Path relative to the phone's HOME." },
          "sha8": { "type": "string" }
        },
        "required": ["installed", "changed", "helper_path", "sha8"]
      }
    },
    {
      "name": "phone_photo",
      "description": "Take one photo with the phone's back or front camera (autofocus settles first, no shutter sound, nothing shown on the phone screen) and save it as a JPEG file; returns its path. Captures whatever the camera sees, including people and private spaces. If a stream is running, returns its newest frame instead.",
      "risk": "high",
      "requires_confirmation": true,
      "permission": "PERMISSION_SCREEN",
      "input": {
        "type": "object",
        "properties": {
          "camera": { "type": "string", "enum": ["back", "front"], "default": "back" },
          "width": { "type": "integer", "enum": [1920, 1280, 800, 640, 320], "description": "Give together with height; supported pairs: 1920x1080, 1280x720 (default), 800x600, 640x480, 320x240." },
          "height": { "type": "integer", "enum": [1080, 720, 600, 480, 240] },
          "af": { "type": "string", "enum": ["video", "picture", "auto", "off"], "default": "video", "description": "Autofocus mode; video converges fastest. The front camera is fixed-focus." },
          "flash": { "type": "boolean", "default": false, "description": "Light the camera torch during capture (brighter, warms the phone)." },
          "quality": { "type": "integer", "minimum": 30, "maximum": 95, "default": 80, "description": "JPEG quality." }
        },
        "additionalProperties": false
      },
      "output": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "width": { "type": "integer" },
          "height": { "type": "integer" },
          "format": { "type": "string" },
          "camera": { "type": "string" },
          "source": { "type": "string", "enum": ["camera", "stream"] },
          "bytes": { "type": "integer" }
        },
        "required": ["path", "format", "camera", "source"]
      }
    },
    {
      "name": "phone_stream_start",
      "description": "Start a live camera stream from the phone; the newest frame is always at latest_path (a JPEG that is replaced atomically), so poll that file to see what the camera sees now. Optionally also records every frame to an MJPEG file. Runs until stopped or max_duration_ms. Frame rate is limited by light: about 12 fps in a dim room, 24+ with light. One stream at a time.",
      "risk": "high",
      "requires_confirmation": true,
      "permission": "PERMISSION_SCREEN",
      "input": {
        "type": "object",
        "properties": {
          "camera": { "type": "string", "enum": ["back", "front"], "default": "back" },
          "width": { "type": "integer", "enum": [1920, 1280, 800, 640, 320] },
          "height": { "type": "integer", "enum": [1080, 720, 600, 480, 240] },
          "fps": { "type": "integer", "minimum": 1, "maximum": 30, "default": 15 },
          "quality": { "type": "integer", "minimum": 30, "maximum": 95, "default": 80 },
          "af": { "type": "string", "enum": ["video", "picture", "auto", "off"], "default": "video" },
          "flash": { "type": "boolean", "default": false },
          "record": { "type": "boolean", "default": false, "description": "Also append every frame to a record-<ms>.mjpg file (about 1.6 MB/s at 720p)." },
          "max_duration_ms": { "type": "integer", "minimum": 1000, "maximum": 1800000, "default": 300000 }
        },
        "additionalProperties": false
      },
      "output": {
        "type": "object",
        "properties": {
          "stream_id": { "type": "string" },
          "latest_path": { "type": "string" },
          "record_path": { "type": ["string", "null"] },
          "camera": { "type": "string" },
          "width": { "type": "integer" },
          "height": { "type": "integer" }
        },
        "required": ["stream_id", "latest_path"]
      }
    },
    {
      "name": "phone_stream_stop",
      "description": "Stop the running camera stream and release the phone's camera; safe to call when nothing is running.",
      "risk": "medium",
      "permission": "PERMISSION_SCREEN",
      "input": {
        "type": "object",
        "properties": { "stream_id": { "type": "string", "description": "Omit to stop the active stream." } },
        "additionalProperties": false
      },
      "output": {
        "type": "object",
        "properties": {
          "stopped": { "type": "boolean" },
          "frames": { "type": "integer" },
          "duration_ms": { "type": "integer" },
          "record_path": { "type": ["string", "null"] },
          "error": { "type": "string", "description": "Why no frame ever arrived, when frames is 0." }
        },
        "required": ["stopped"]
      }
    },
    {
      "name": "phone_stream_status",
      "description": "Report the running camera stream: frames so far, measured fps, and how stale the newest frame is; active:false when nothing runs.",
      "risk": "low",
      "permission": "PERMISSION_SCREEN",
      "input": { "type": "object", "properties": {}, "additionalProperties": false },
      "output": {
        "type": "object",
        "properties": {
          "active": { "type": "boolean" },
          "stream_id": { "type": "string" },
          "camera": { "type": "string" },
          "frames": { "type": "integer" },
          "fps_measured": { "type": "number" },
          "last_frame_age_ms": { "type": ["integer", "null"] },
          "latest_path": { "type": "string" }
        },
        "required": ["active"]
      }
    }
  ],
  "config_schema": {
    "$schema": "http://json-schema.org/draft-07/schema#",
    "type": "object",
    "properties": {
      "PHONE_PLUGIN_TRANSPORT": { "type": "string", "enum": ["ssh", "local"], "default": "ssh", "description": "ssh: the phone is a remote node reached with ssh; local: the kernel runs on the phone itself." },
      "PHONE_PLUGIN_SSH_HOST": { "type": "string", "default": "mi6", "description": "ssh host alias from the operator's ssh config (key auth only). Characters [A-Za-z0-9._-]." },
      "PHONE_PLUGIN_SSH_MUX": { "type": "string", "default": "1", "description": "Set to 0 to disable ssh ControlMaster connection sharing." },
      "PHONE_PLUGIN_REMOTE_UID": { "type": "string", "default": "32011", "description": "uid of the phone user; selects /run/user/<uid> for the camera environment." },
      "PHONE_PLUGIN_REMOTE_DIR": { "type": "string", "default": ".local/share/vyn-phone", "description": "Directory for the helper script, relative to the phone user's HOME." },
      "PHONE_PLUGIN_DIR": { "type": "string", "description": "Local data dir for photos, latest.jpg and records (mode 0700). Default ~/.local/share/vyn/phone/." }
    },
    "additionalProperties": true
  }
}
```

```rust file=plugins/phone/src/main.rs
//! `phone` plugin — camera of a phone reached over ssh (or run on the phone
//! itself). See README.md and docs/superpowers/specs/2026-09-29-phone-plugin-design.md.
//!
//! ## Concurrency
//!
//! Uses the SDK's concurrent loop ([`ConcurrentHandler`] + [`serve_concurrent`]),
//! not a sequential one: `phone_photo` blocks for seconds on a network round
//! trip, and a sequential loop would stall the kernel's `Ping` and every other
//! request for that long. The plugin makes no outbound plugin calls, so no
//! RPC proxy is needed.

use std::sync::Arc;

use phone_plugin::config::Config;
use phone_plugin::error::PhoneError;
use phone_plugin::params::{parse_stream_id, PhotoParams, StreamParams};
use phone_plugin::stream::StreamRegistry;
use phone_plugin::transport::{self, Transport};
use phone_plugin::{helper, photo, status};
use serde_json::{json, Value};
use vynkor_sdk::concurrent::{response_envelope, serve_concurrent};
use vynkor_sdk::proto::{ActionRequest, Envelope, PluginManifest};
use vynkor_sdk::{ConcurrentHandler, VynkorClient, VynkorError};

const PLUGIN_ID: &str = "phone";
const PLUGIN_VERSION: &str = "0.1.0";

const ACTIONS: [&str; 6] = [
    "phone_status",
    "phone_setup",
    "phone_photo",
    "phone_stream_start",
    "phone_stream_stop",
    "phone_stream_status",
];

struct App {
    transport: Arc<dyn Transport>,
    cfg: Config,
    reg: StreamRegistry,
}

impl App {
    fn new(cfg: Config, transport: Arc<dyn Transport>) -> Self {
        Self { transport, cfg, reg: StreamRegistry::new() }
    }

    async fn dispatch(&self, action: &str, params: &Value) -> Result<Value, PhoneError> {
        let t = self.transport.as_ref();
        match action {
            "phone_status" => Ok(status::status(t, &self.cfg, &self.reg).await),
            "phone_setup" => {
                let r = helper::setup(t, &self.cfg).await?;
                Ok(json!({
                    "installed": r.installed, "changed": r.changed,
                    "helper_path": r.helper_path, "sha8": r.sha8,
                }))
            }
            "phone_photo" => photo::take(t, &self.cfg, &self.reg, &PhotoParams::parse(params)?).await,
            "phone_stream_start" => self.reg.start(t, &self.cfg, &StreamParams::parse(params)?).await,
            "phone_stream_stop" => self.reg.stop(parse_stream_id(params)?).await,
            "phone_stream_status" => Ok(self.reg.status().await),
            other => Err(PhoneError::BadParams(format!("unknown action: {other}"))),
        }
    }
}

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_NETWORK".to_string(), "PERMISSION_SCREEN".to_string()],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
        action_specs: vynkor_plugin_manifest::action_specs(),
        ..Default::default()
    }
}

impl ConcurrentHandler for App {
    fn id(&self) -> &str {
        PLUGIN_ID
    }

    fn version(&self) -> &str {
        PLUGIN_VERSION
    }

    fn manifest(&self) -> PluginManifest {
        manifest()
    }

    async fn on_action(&self, req: ActionRequest) -> Vec<Envelope> {
        let params: Value = if req.params_json.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice(&req.params_json) {
                Ok(v) => v,
                Err(e) => {
                    return vec![response_envelope(req.action_id, Err(format!("invalid params_json: {e}")))];
                }
            }
        };
        let wire = self
            .dispatch(&req.action, &params)
            .await
            .map(|v| v.to_string().into_bytes())
            .map_err(|e| e.to_string());
        vec![response_envelope(req.action_id, wire)]
    }
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[{PLUGIN_ID}] bad configuration: {e}");
            std::process::exit(2);
        }
    };
    let transport = transport::from_config(&cfg);
    eprintln!("[{PLUGIN_ID}] transport {}", transport.describe());
    let app = Arc::new(App::new(cfg, transport));
    let client = VynkorClient::connect_from_env().await?;
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    serve_concurrent(client, &jwt_token, app).await?;
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phone_plugin::framing::encode_frame;
    use phone_plugin::transport::fake::FakeTransport;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use vynkor_sdk::concurrent::run_concurrent_loop;
    use vynkor_sdk::proto::{envelope, ActionStatus, PluginShutdown};

    fn action_request(action_id: &str, action: &str, params: Value) -> Envelope {
        Envelope {
            payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                action_id: action_id.to_string(),
                action: action.to_string(),
                params_json: serde_json::to_vec(&params).unwrap(),
                timeout_ms: 0,
                streaming: false,
                caller_plugin_id: "tester".into(),
            })),
            ..Default::default()
        }
    }

    async fn call(kernel: &mut VynkorClient, id: &str, action: &str, params: Value) -> Result<Value, String> {
        kernel.send("phone", action_request(id, action, params)).await.unwrap();
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for plugin reply")
                .unwrap();
            if let Some(envelope::Payload::ActionResponse(resp)) = env.payload {
                if resp.action_id == id {
                    return if resp.status == ActionStatus::ActionOk as i32 {
                        serde_json::from_slice::<Value>(&resp.data_json).map_err(|e| format!("malformed payload: {e}"))
                    } else {
                        Err(resp.error)
                    };
                }
            }
        }
    }

    fn test_cfg(dir: &std::path::Path) -> Config {
        Config::from_lookup(|k| match k {
            "PHONE_PLUGIN_DIR" => Some(dir.to_string_lossy().into_owned()),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn shipped_manifest_matches_the_served_actions_and_documents_each() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        let mut declared: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        declared.sort();
        let mut served: Vec<String> = ACTIONS.iter().map(|s| s.to_string()).collect();
        served.sort();
        assert_eq!(declared, served, "plugin.json and ACTIONS must list exactly the same actions");
        let undocumented: Vec<&str> = specs.iter().filter(|s| s.description.is_empty()).map(|s| s.name.as_str()).collect();
        assert!(undocumented.is_empty(), "actions missing a description: {undocumented:?}");
        for s in &specs {
            assert!(s.risk != 0, "{} must declare a risk", s.name);
        }
        let high: Vec<&str> = specs.iter().filter(|s| s.requires_confirmation).map(|s| s.name.as_str()).collect();
        assert!(high.contains(&"phone_photo") && high.contains(&"phone_stream_start"), "{high:?}");
    }

    #[test]
    fn manifest_declares_the_two_permissions() {
        assert_eq!(manifest().permissions, vec!["PERMISSION_NETWORK", "PERMISSION_SCREEN"]);
    }

    #[tokio::test]
    async fn e2e_status_setup_photo_and_error_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTransport::new());
        let jpeg = vec![0xFF, 0xD8, 7, 7, 0xFF, 0xD9];
        fake.push_run(FakeTransport::ok(Vec::new())); // status: helper missing
        fake.push_run(FakeTransport::ok(Vec::new())); // setup: check -> missing
        fake.push_run(FakeTransport::ok(Vec::new())); // setup: deploy
        fake.push_run(FakeTransport::ok(encode_frame(&jpeg))); // photo

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        let app = Arc::new(App::new(test_cfg(dir.path()), fake.clone() as Arc<dyn Transport>));
        let loop_task = tokio::spawn(run_concurrent_loop(client, app));

        let v = call(&mut kernel, "t-1", "phone_status", json!({})).await.unwrap();
        assert_eq!(v["helper"], json!("missing"));
        assert_eq!(v["reachable"], json!(true));

        let v = call(&mut kernel, "t-2", "phone_setup", json!({})).await.unwrap();
        assert_eq!(v["changed"], json!(true));

        let v = call(&mut kernel, "t-3", "phone_photo", json!({"camera": "front", "width": 640, "height": 480})).await.unwrap();
        assert_eq!(std::fs::read(v["path"].as_str().unwrap()).unwrap(), jpeg);
        assert_eq!(v["camera"], json!("front"));

        // bad params and unknown actions come back as coded errors, not crashes
        let e = call(&mut kernel, "t-4", "phone_photo", json!({"camera": "side"})).await.unwrap_err();
        assert!(e.starts_with("ERR_PHONE_BAD_PARAMS"), "{e}");
        let e = call(&mut kernel, "t-5", "phone_nope", json!({})).await.unwrap_err();
        assert!(e.contains("unknown action"), "{e}");

        let v = call(&mut kernel, "t-6", "phone_stream_status", json!({})).await.unwrap();
        assert_eq!(v, json!({"active": false}));
        let v = call(&mut kernel, "t-7", "phone_stream_stop", json!({})).await.unwrap();
        assert_eq!(v["stopped"], json!(false));

        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown { reason: "test done".into(), grace_seconds: 0 })),
            ..Default::default()
        };
        kernel.send("phone", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task).await.expect("loop did not exit").unwrap().unwrap();
    }
}
```

- [ ] **Step 2: Extract, enable all modules, run the whole suite**

```bash
python3 $SCRATCH/extract_plan.py docs/superpowers/plans/2026-09-29-phone-plugin.md plugins/phone/src/status.rs plugins/phone/plugin.json plugins/phone/src/main.rs plugins/phone/src/lib.rs
cargo test --manifest-path plugins/phone/Cargo.toml
cargo clippy --manifest-path plugins/phone/Cargo.toml --all-targets -- -D warnings
```

Expected: all unit tests plus the e2e fake-kernel test pass; clippy clean. (`status.rs` contains two throw-away `let _ =` lines; remove them and the unused imports if clippy complains — they exist only to keep imports honest until the final cleanup, delete them in this step.)

- [ ] **Step 3: Commit** — `git add plugins/phone && git commit -m "feat(phone): status action, plugin.json, binary and fake-kernel e2e test"`

---

## Task 7: Docs, config example, live smoke test, live verification

**Files:** Create `plugins/phone/README.md`, `plugins/phone/ROADMAP.md`, `plugins/phone/config.example.yaml`, `plugins/phone/tests/live.rs`

- [ ] **Step 1: Extract the files**

```markdown file=plugins/phone/README.md
# phone plugin

Camera of a phone (built for a Xiaomi Mi 6 on Ubuntu Touch) for vynkor: take a
photo or run a live stream from the back or front camera **without any window,
shutter sound or screen capture**. The phone is reached over ssh, or — when the
kernel runs on the phone itself — locally.

Permissions: `PERMISSION_NETWORK` (status, setup), `PERMISSION_SCREEN` (camera).
Camera actions are risk `high` with `requires_confirmation`.

## Actions

| Action | Params | Result |
|---|---|---|
| `phone_status` | — | `{transport, host, reachable, helper: ok\|stale\|missing\|unknown, stream_active, latency_ms}` |
| `phone_setup` | — | `{installed, changed, helper_path, sha8}` — installs `hybcam-<sha8>.py` into the phone's HOME (idempotent) |
| `phone_photo` | `camera?` back\|front, `width?`+`height?` (1920x1080, **1280x720**, 800x600, 640x480, 320x240), `af?` video\|picture\|auto\|off, `flash?`, `quality?` 30–95 | `{path, width, height, format, camera, source: camera\|stream, bytes}` |
| `phone_stream_start` | as photo + `fps?` 1–30, `record?`, `max_duration_ms?` (≤ 1800000) | `{stream_id, latest_path, record_path, camera, width, height}` |
| `phone_stream_stop` | `stream_id?` | `{stopped, frames, duration_ms, record_path}` |
| `phone_stream_status` | — | `{active, stream_id, frames, fps_measured, last_frame_age_ms, camera, latest_path}` |

Frames are files: `PHONE_PLUGIN_DIR` (default `~/.local/share/vyn/phone/`, mode 0700)
holds `photo-<ms>.jpg`, `latest.jpg` (replaced atomically per frame — poll it to
see the camera live) and, with `record: true`, `record-<ms>.mjpg`.

## How it works

`libcamera.so.1` (libhybris camera compat layer) is driven from a ~150-line
Python helper through `ctypes`; the preview callback delivers NV21 frames in
memory and `cv2` on the phone encodes JPEG, so only ~50–200 KB/frame crosses
the link. The helper needs a dummy preview texture id
(`android_camera_set_preview_texture(ctl, 1)`): without a preview target the
HAL waits for a window and sends nothing.

## Limits (measured on the Mi 6)

- Frame rate is limited by **exposure**: ~12 fps in a dim room, ~24 with light
  (`flash: true` lights the camera torch). Not by ssh or CPU; MJPEG holds it over wifi at 640x480, 1280x720 and 1920x1080.
- No fps-range / exposure control exists in `libcamera.so.1`.
- One camera client at a time (HAL). Photos are serialized; a photo during a
  stream returns the newest stream frame (`source: "stream"`).
- Back camera has autofocus; front is fixed-focus.
- If another app holds the camera (`lomiri-camera-app`), actions fail with
  `ERR_PHONE_CAMERA`.

## Errors

`ERR_PHONE_BAD_PARAMS`, `ERR_PHONE_UNREACHABLE` (ssh 255 / timeout),
`ERR_PHONE_BUSY`, `ERR_PHONE_HELPER_MISSING` (run `phone_setup`),
`ERR_PHONE_CAMERA`, `ERR_PHONE_BACKEND`.

## Configuration

See `config.example.yaml` and `plugin.json` `config_schema`. Variables:
`PHONE_PLUGIN_TRANSPORT` (`ssh`|`local`), `PHONE_PLUGIN_SSH_HOST` (default `mi6`),
`PHONE_PLUGIN_SSH_MUX`, `PHONE_PLUGIN_REMOTE_UID` (default 32011),
`PHONE_PLUGIN_REMOTE_DIR`, `PHONE_PLUGIN_DIR`.

## Watching live as a human

```bash
ssh mi6 'export XDG_RUNTIME_DIR=/run/user/32011 MIR_SOCKET=/run/user/32011/mir_socket_trusted; \
  python3 ~/.local/share/vyn-phone/hybcam-*.py --cam back -W 1280 -H 720 --fps 30 --secs 3600 --fmt jpeg --af video --out -' \
  | ffplay -f mjpeg -fflags nobuffer -flags low_delay -i -
```

## Testing

`cargo test --manifest-path plugins/phone/Cargo.toml` — unit tests plus a
fake-kernel round trip; no phone needed. Live smoke test against a real phone:
`PHONE_PLUGIN_LIVE=1 cargo test --manifest-path plugins/phone/Cargo.toml --test live -- --ignored --nocapture`.
```

```markdown file=plugins/phone/ROADMAP.md
# phone — roadmap

Shipped: v0.1.0 — transport (ssh/local), status, setup, photo, stream.

## Next (each its own version, spec §Layer 3/4)

- **v0.2.0 audio**: `phone_mic_start/stop` (PCM through `AudioStreamChunk`,
  allowlisted targets like `mic`), `phone_speak` (`paplay`, 48 kHz sink,
  wrapped in `timeout`). With the kernel on the phone the existing `mic`/`sound`
  plugins may cover this; verify before building.
- **v0.3.0 hardware**: `phone_led`, `phone_torch`, `phone_vibrate` (root sysfs via
  `sudo -S` with the password from a 0600 file on stdin, never argv),
  `phone_battery` (read-only; the `charge-limit` user service keeps owning the limit).

## Non-goals

- No fps/exposure control (the API has none), no daemon on the phone, no video
  wire protocol (frames are files), no generic multi-device abstraction until a
  second device exists.
```

```yaml file=plugins/phone/config.example.yaml
# Example kernel config.yaml entry for the `phone` plugin (v0.1.0).
# Copy this block into the top-level `plugins:` list of the kernel's config.yaml.
#
# Permissions: PERMISSION_NETWORK (ssh) + PERMISSION_SCREEN (camera).
# The plugin spawns `ssh` (transport ssh) or the helper directly (transport
# local) with argv only — never a local shell.
#
# Setup:
#   1. ~/.ssh/config: a `Host mi6` entry with key auth (BatchMode is used).
#   2. Run the `phone_setup` action once: it installs the camera helper on the phone.
#
# Sandbox:
#   * Kernel on the laptop: ssh needs ~/.ssh and the network. If the sandbox blocks
#     that, set `sandbox: false` for this plugin.
#   * Kernel ON THE PHONE: `sandbox: false` is required — the phone's kernel
#     (4.4.153-Halium) has no Landlock, and the sandbox fails closed without it.

plugins:
  - id: phone
    binary: /opt/plugins/phone
    restart: on-failure
    max_restarts: 5
    sandbox: false
    env:
      # ssh (default): the phone is a remote node.  local: the kernel runs on the phone.
      # - PHONE_PLUGIN_TRANSPORT=ssh
      # - PHONE_PLUGIN_SSH_HOST=mi6
      # - PHONE_PLUGIN_SSH_MUX=1
      # uid of the phone user (selects /run/user/<uid> for the camera):
      # - PHONE_PLUGIN_REMOTE_UID=32011
      # Helper directory, relative to the phone user's HOME:
      # - PHONE_PLUGIN_REMOTE_DIR=.local/share/vyn-phone
      # Photos, latest.jpg and records (mode 0700):
      # - PHONE_PLUGIN_DIR=/var/lib/vyn/phone
    grace_seconds: 10
    max_procs: 64
    max_vmem_mb: 512
```

```rust file=plugins/phone/tests/live.rs
//! Live smoke test against a real phone. Ignored by default; run with
//!   PHONE_PLUGIN_LIVE=1 cargo test --manifest-path plugins/phone/Cargo.toml --test live -- --ignored --nocapture
//! Uses the same env as the plugin (PHONE_PLUGIN_SSH_HOST, ...). Takes a back and a
//! front photo and runs a 3 s stream; the phone camera must be free.

use std::time::Duration;

use phone_plugin::config::Config;
use phone_plugin::framing::is_jpeg;
use phone_plugin::params::{PhotoParams, StreamParams};
use phone_plugin::stream::StreamRegistry;
use phone_plugin::{helper, photo, status, transport};
use serde_json::json;

#[tokio::test]
#[ignore]
async fn live_status_setup_photos_and_stream() {
    if std::env::var("PHONE_PLUGIN_LIVE").as_deref() != Ok("1") {
        eprintln!("PHONE_PLUGIN_LIVE!=1; skipping");
        return;
    }
    let cfg = Config::from_env().unwrap();
    cfg.ensure_dir().unwrap();
    let t = transport::from_config(&cfg);
    let reg = StreamRegistry::new();

    let s = status::status(t.as_ref(), &cfg, &reg).await;
    println!("status: {s}");
    assert_eq!(s["reachable"], json!(true), "phone unreachable: {s}");

    let r = helper::setup(t.as_ref(), &cfg).await.unwrap();
    println!("setup: {r:?}");

    for cam in ["back", "front"] {
        let p = PhotoParams::parse(&json!({"camera": cam, "width": 640, "height": 480})).unwrap();
        let v = photo::take(t.as_ref(), &cfg, &reg, &p).await.unwrap();
        println!("photo {cam}: {v}");
        let bytes = std::fs::read(v["path"].as_str().unwrap()).unwrap();
        assert!(is_jpeg(&bytes), "{cam} photo is not a JPEG");
        assert!(bytes.len() > 5_000, "{cam} photo suspiciously small: {}", bytes.len());
    }

    let sp = StreamParams::parse(&json!({"width": 640, "height": 480, "fps": 15, "flash": false})).unwrap();
    let started = reg.start(t.as_ref(), &cfg, &sp).await.unwrap();
    println!("stream: {started}");
    tokio::time::sleep(Duration::from_secs(4)).await;
    let st = reg.status().await;
    println!("stream status: {st}");
    assert!(st["frames"].as_u64().unwrap_or(0) >= 10, "too few frames: {st}");
    let stopped = reg.stop(None).await.unwrap();
    println!("stopped: {stopped}");
    assert_eq!(stopped["stopped"], json!(true));
}
```

- [ ] **Step 2: Extract, run unit tests, then the live smoke test against the connected phone**

```bash
python3 $SCRATCH/extract_plan.py docs/superpowers/plans/2026-09-29-phone-plugin.md plugins/phone/README.md plugins/phone/ROADMAP.md plugins/phone/config.example.yaml plugins/phone/tests/live.rs
cargo test --manifest-path plugins/phone/Cargo.toml
PHONE_PLUGIN_LIVE=1 cargo test --manifest-path plugins/phone/Cargo.toml --test live -- --ignored --nocapture
```

Expected: unit tests pass; live test prints status / setup / two photo paths / a stream with ≥ 10 frames and stops cleanly. Fixes discovered here (ssh ControlMaster interplay with piped stdio, helper edge cases) are made in the owning file **and** in this plan, then re-tested.

- [ ] **Step 3: Commit** — `git add plugins/phone && git commit -m "docs(phone): README, ROADMAP, config example and live smoke test"`

---

## Self-review (spec coverage)

- Transport `Ssh|Local|Fake`, quoting, host validation → Tasks 1–3. `SshTransport` argv (BatchMode, ControlMaster) → Task 3.
- Helper with `--framing len32`, `--snap`, dead-man → Task 4; hashed deploy, idempotent `phone_setup` → Task 4.
- Actions `status/setup/photo/stream_*` with exact params/results → Tasks 5–6; manifest lists only implemented actions, checked against `ACTIONS` by a test → Task 6.
- Camera ownership rules (busy, photo-during-stream, serialize) → Task 5.
- Storage (0700 dir, atomic `latest.jpg`, record) → Tasks 1, 5.
- Errors taxonomy and mapping → Task 1, exercised in Tasks 5–6.
- Permissions / risk / confirmation → Task 6 (`plugin.json` + test).
- Sandbox note, config example, docs, live smoke test → Task 7.
- Layers 3–4: intentionally out of v0.1 (ROADMAP).

Types used across tasks: `RemoteCmd`, `Transport::run/spawn/describe`, `Output`, `Control`, `Spawned`, `Config` fields (`transport, ssh_host, ssh_mux, remote_uid, remote_dir, dir`), `PhotoParams`/`StreamParams` fields, `StreamRegistry::{new,start,stop,status,active_camera,is_active,camera_guard}`, `helper::{helper_cmd, remote_path, file_name, sha8, check, setup, parse_state}` — signatures are identical everywhere they appear.
