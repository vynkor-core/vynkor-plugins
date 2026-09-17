# `capture` plugin (v1: screenshot + screen record + OCR) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a new `plugins/capture/` plugin that takes screenshots, records
screen video, and runs local OCR, using host-binary spawn chains so it works
across wlroots (Hyprland/sway), GNOME, KDE, and X11 without any kernel
change (single `PERMISSION_SCREEN`).

**Architecture:** Sequential `Plugin::serve()` loop (same shape as `sound`/
`clipboard` — low-volume, spawn-bound, no need for the SDK's
`ConcurrentHandler`). A `Spawner` trait (copied in spirit from
`sound::players::Spawner`) is the process-execution boundary so every test
runs against a `FakeSpawner`, never a real host binary. Per-capability
backend chains (screenshot, record) try candidates in order and fall
through on `ErrorKind::NotFound`; a `capture_status` action reports what
was actually found via a plain `$PATH` scan (no eager startup probing).

**Tech Stack:** Rust, `vynkor-sdk` 0.0.3, `tokio` (process + time), `zbus` 4
(portal screenshot fallback only), `serde_json`, `base64`, `libc` (SIGINT
to a running recorder for a clean container finalize).

**Spec:** `docs/superpowers/specs/2026-09-17-capture-plugin-design.md`

## Global Constraints

- Every host binary spawn is argv-only, **never** a shell — same rule as
  `clipboard`/`notify`/`sound`.
- Single permission: `PERMISSION_SCREEN`. No new proto/permission work.
- All output artifacts (screenshots, recordings) are written under
  `CAPTURE_PLUGIN_DIR` (default `~/.local/share/vynkor/capture/`) and
  returned as an absolute path — never inline base64 on output.
- `region` param across `capture_screenshot`/`capture_record_start` is one
  of: `"full"` (default), `"select"` (interactive), or
  `{"x":int,"y":int,"w":int,"h":int}` (explicit rect). **Scope cut from the
  spec's draft:** `{"monitor": N}` is dropped — there is no
  compositor-agnostic way to resolve a numeric monitor index to a `grim -o`
  output name without a Hyprland-specific IPC call, which would break the
  "many Linux" goal. `"select"` covers the "just this screen" case in
  practice.
- Error codes: `ERR_CAPTURE_BAD_PARAMS`, `ERR_CAPTURE_NOT_SUPPORTED`,
  `ERR_CAPTURE_BUSY`, `ERR_CAPTURE_CANCELLED`, `ERR_CAPTURE_BACKEND`
  (defined in Task 2, used by every later task — do not invent new codes).
- `vynkor-sdk = "0.0.3"` (pinned version every other plugin in this repo
  uses — do not bump).

---

### Task 1: Crate scaffold + walking-skeleton serve loop

**Files:**
- Create: `plugins/capture/Cargo.toml`
- Create: `plugins/capture/src/lib.rs`
- Create: `plugins/capture/src/main.rs`

**Interfaces:**
- Produces: `PLUGIN_ID = "capture"`, `PLUGIN_VERSION = "0.1.0"`, the
  `manifest()` fn (grows in later tasks), `serve(client, app)`,
  `handle_action_request(app, req) -> ActionResponse` — every later task
  adds a match arm to this function's `match req.action.as_str()` block.

- [ ] **Step 1: Write `Cargo.toml`**

```toml
[package]
name = "capture-plugin"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
name = "capture_plugin"
path = "src/lib.rs"

[[bin]]
name = "capture"
path = "src/main.rs"

[dependencies]
vynkor-sdk = "0.0.3"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "process", "time"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
async-trait = "0.1"
base64 = "0.22"
libc = "0.2"
zbus = { version = "4", default-features = false, features = ["tokio"] }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write `src/lib.rs`**

```rust
//! `capture` plugin — screen capture (screenshot, video record) + local
//! OCR for vynkor plugins. Every backend is a host binary spawned by argv,
//! never a shell. See README.md for the full backend-chain table.

pub mod error;
```

(Later tasks add `pub mod paths; pub mod session; pub mod spawner; pub mod
screenshot; pub mod portal; pub mod record; pub mod ocr;` to this same
file — one `pub mod` line per task, appended in order.)

- [ ] **Step 3: Write `src/main.rs`** (walking skeleton — only
  `capture_status` wired, returning a static stub so the wire protocol is
  provably correct before any real backend code exists)

```rust
mod handler_stub {}

use std::sync::Arc;

use serde_json::Value;
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, PluginManifest,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "capture";
const PLUGIN_VERSION: &str = "0.1.0";

struct App;

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_SCREEN".to_string()],
        actions: vec!["capture_status".to_string()],
        ..Default::default()
    }
}

async fn serve(mut client: VynkorClient, app: Arc<App>) -> Result<(), VynkorError> {
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client
        .register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token)
        .await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!(
            "registration rejected: {}",
            ack.reject_reason
        )));
    }
    println!("[{PLUGIN_ID}] registered with kernel");

    loop {
        let env = match client.recv().await {
            Ok(env) => env,
            Err(_) => break,
        };
        match env.payload {
            Some(envelope::Payload::Ping(ping)) => {
                let pong = Envelope {
                    payload: Some(envelope::Payload::Pong(vynkor_sdk::proto::Pong {
                        original_timestamp: ping.timestamp,
                        server_timestamp: unix_millis(),
                    })),
                    ..Default::default()
                };
                let _ = client.send("kernel", pong).await;
            }
            Some(envelope::Payload::PluginShutdown(_)) => break,
            Some(envelope::Payload::Event(event)) => {
                let _ = client.ack_event(&event.event_id).await;
            }
            Some(envelope::Payload::ActionRequest(req)) => {
                let response = handle_action_request(&app, req).await;
                let _ = client
                    .send(
                        "kernel",
                        Envelope {
                            payload: Some(envelope::Payload::ActionResponse(response)),
                            ..Default::default()
                        },
                    )
                    .await;
            }
            _ => {}
        }
    }
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

async fn handle_action_request(_app: &App, req: ActionRequest) -> ActionResponse {
    let result: Result<Value, String> = match req.action.as_str() {
        "capture_status" => Ok(serde_json::json!({
            "session_type": "unknown",
            "screenshot_backend": null,
            "record_backend": null,
            "ocr_available": false,
            "portal_available": false
        })),
        other => {
            return ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionNotFound as i32,
                data_json: Vec::new(),
                error: format!("unknown action: {other}"),
            };
        }
    };

    match result {
        Ok(data) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionOk as i32,
            data_json: data.to_string().into_bytes(),
            error: String::new(),
        },
        Err(error) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionError as i32,
            data_json: Vec::new(),
            error,
        },
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let app = Arc::new(App);
    let client = VynkorClient::connect_from_env().await?;
    serve(client, app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
    use vynkor_sdk::proto::PluginRegisterAck;

    type Replies = Arc<AsyncMutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

    enum Cmd {
        Call { action: String, params: Value, reply: oneshot::Sender<Result<Value, String>> },
    }

    struct Shim {
        tx: mpsc::Sender<Cmd>,
    }

    impl Shim {
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.tx
                .send(Cmd::Call { action: action.to_string(), params, reply: reply_tx })
                .await
                .expect("shim loop died");
            tokio::time::timeout(Duration::from_secs(5), reply_rx)
                .await
                .expect("timed out waiting for plugin reply")
                .expect("shim dropped reply channel")
        }
    }

    async fn start_plugin() -> Shim {
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        let app = Arc::new(App);
        tokio::spawn(async move {
            let _ = serve(plugin_client, app).await;
        });

        let (tx, rx) = mpsc::channel::<Cmd>(16);
        let replies: Replies = Arc::new(AsyncMutex::new(HashMap::new()));
        tokio::spawn(run_shim(kernel_client, rx, replies));
        Shim { tx }
    }

    async fn run_shim(mut kernel: VynkorClient, mut rx: mpsc::Receiver<Cmd>, replies: Replies) {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for plugin registration")
                .expect("plugin stream closed before registration");
            if matches!(env.payload, Some(envelope::Payload::PluginRegister(_))) {
                let _ = kernel
                    .send(
                        "capture",
                        Envelope {
                            payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck {
                                accepted: true,
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                    )
                    .await;
                break;
            }
        }

        let mut seq: u64 = 0;
        loop {
            tokio::select! {
                env = kernel.recv() => {
                    let env = match env { Ok(e) => e, Err(_) => break };
                    if let Some(envelope::Payload::ActionResponse(resp)) = env.payload {
                        let mut pending = replies.lock().await;
                        if let Some(tx) = pending.remove(&resp.action_id) {
                            let result = if resp.status == ActionStatus::ActionOk as i32 {
                                serde_json::from_slice::<Value>(&resp.data_json)
                                    .map_err(|e| format!("malformed payload: {e}"))
                            } else {
                                Err(resp.error)
                            };
                            let _ = tx.send(result);
                        }
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Call { action, params, reply }) => {
                            seq += 1;
                            let action_id = format!("t-{seq}");
                            replies.lock().await.insert(action_id.clone(), reply);
                            let _ = kernel.send("capture", Envelope {
                                payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                                    action_id,
                                    action,
                                    params_json: serde_json::to_vec(&params).unwrap(),
                                    timeout_ms: 0,
                                    streaming: false,
                                    caller_plugin_id: "tester".into(),
                                })),
                                ..Default::default()
                            }).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn e2e_capture_status_round_trip() {
        let shim = start_plugin().await;
        let v = shim.call("capture_status", serde_json::json!({})).await.unwrap();
        assert_eq!(v["ocr_available"], false);
    }

    #[tokio::test]
    async fn e2e_unknown_action_is_not_found() {
        let shim = start_plugin().await;
        let err = shim.call("capture_bogus", serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cd plugins/capture && cargo test`
Expected: PASS (2 tests: `e2e_capture_status_round_trip`,
`e2e_unknown_action_is_not_found`)

- [ ] **Step 5: Commit**

```bash
git add plugins/capture/Cargo.toml plugins/capture/src/lib.rs plugins/capture/src/main.rs
git commit -m "feat(capture): scaffold plugin with capture_status walking skeleton"
```

---

### Task 2: Error taxonomy

**Files:**
- Create: `plugins/capture/src/error.rs`
- Modify: `plugins/capture/src/lib.rs` (already has `pub mod error;` from
  Task 1)

**Interfaces:**
- Consumes: nothing (leaf module)
- Produces: `pub enum CaptureErrorCode { BadParams, NotSupported, Busy,
  Cancelled, Backend }` with `as_str(self) -> &'static str`; `pub enum
  CaptureError { BadParams(String), NotSupported(&'static str), Busy,
  Cancelled(&'static str), Backend(String) }` implementing
  `std::error::Error` + `Display` (via `thiserror`). Later tasks return
  `Result<Value, CaptureError>` from their handlers and convert to
  `String` at the `main.rs` dispatch boundary via `.to_string()`.

- [ ] **Step 1: Add `thiserror` dependency**

Edit `plugins/capture/Cargo.toml`, add under `[dependencies]`:

```toml
thiserror = "1"
```

- [ ] **Step 2: Write `src/error.rs`**

```rust
//! Typed error taxonomy for the `capture` plugin. Every error carries a
//! stable `ERR_CAPTURE_*` code prefix (same convention as `system`'s
//! `ERR_SYS_*`), so callers can branch on the code without parsing prose.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureErrorCode {
    BadParams,
    NotSupported,
    Busy,
    Cancelled,
    Backend,
}

impl CaptureErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            CaptureErrorCode::BadParams => "ERR_CAPTURE_BAD_PARAMS",
            CaptureErrorCode::NotSupported => "ERR_CAPTURE_NOT_SUPPORTED",
            CaptureErrorCode::Busy => "ERR_CAPTURE_BUSY",
            CaptureErrorCode::Cancelled => "ERR_CAPTURE_CANCELLED",
            CaptureErrorCode::Backend => "ERR_CAPTURE_BACKEND",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("ERR_CAPTURE_BAD_PARAMS: {0}")]
    BadParams(String),

    /// No backend for this capability was found on the host after trying
    /// every candidate in the chain. Detail names the capability.
    #[error("ERR_CAPTURE_NOT_SUPPORTED: {0} is not available on this system")]
    NotSupported(&'static str),

    /// A recording is already in progress (single active slot).
    #[error("ERR_CAPTURE_BUSY: a recording is already in progress")]
    Busy,

    /// An interactive selection (slurp/slop/maim -s/import/gnome-screenshot
    /// -a/spectacle -r) was dismissed by the user. Detail names the tool.
    #[error("ERR_CAPTURE_CANCELLED: {0} selection was cancelled")]
    Cancelled(&'static str),

    /// A detected backend was spawned but failed (nonzero exit, unparseable
    /// output, D-Bus error). Detail carries the cause.
    #[error("ERR_CAPTURE_BACKEND: {0}")]
    Backend(String),
}

impl CaptureError {
    pub const fn code(&self) -> CaptureErrorCode {
        match self {
            CaptureError::BadParams(_) => CaptureErrorCode::BadParams,
            CaptureError::NotSupported(_) => CaptureErrorCode::NotSupported,
            CaptureError::Busy => CaptureErrorCode::Busy,
            CaptureError::Cancelled(_) => CaptureErrorCode::Cancelled,
            CaptureError::Backend(_) => CaptureErrorCode::Backend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_per_variant() {
        assert_eq!(CaptureError::BadParams("x".into()).code().as_str(), "ERR_CAPTURE_BAD_PARAMS");
        assert_eq!(CaptureError::NotSupported("record").code().as_str(), "ERR_CAPTURE_NOT_SUPPORTED");
        assert_eq!(CaptureError::Busy.code().as_str(), "ERR_CAPTURE_BUSY");
        assert_eq!(CaptureError::Cancelled("slurp").code().as_str(), "ERR_CAPTURE_CANCELLED");
        assert_eq!(CaptureError::Backend("boom".into()).code().as_str(), "ERR_CAPTURE_BACKEND");
    }

    #[test]
    fn display_carries_code_and_detail() {
        let e = CaptureError::NotSupported("record");
        assert_eq!(e.to_string(), "ERR_CAPTURE_NOT_SUPPORTED: record is not available on this system");
        let e = CaptureError::Cancelled("slurp");
        assert_eq!(e.to_string(), "ERR_CAPTURE_CANCELLED: slurp selection was cancelled");
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cd plugins/capture && cargo test error::`
Expected: PASS (2 tests)

- [ ] **Step 4: Commit**

```bash
git add plugins/capture/Cargo.toml plugins/capture/src/error.rs
git commit -m "feat(capture): add ERR_CAPTURE_* error taxonomy"
```

---

### Task 3: Data dir + filename generation

**Files:**
- Create: `plugins/capture/src/paths.rs`
- Modify: `plugins/capture/src/lib.rs` — append `pub mod paths;`

**Interfaces:**
- Consumes: nothing
- Produces: `pub const DIR_ENV: &str = "CAPTURE_PLUGIN_DIR";`, `pub fn
  data_dir() -> std::path::PathBuf` (resolves env var, falls back to
  `~/.local/share/vynkor/capture/`, creates it if missing), `pub fn
  screenshot_filename(ext: &str) -> String` → `"screenshot-<unix_millis>.<ext>"`,
  `pub fn record_filename(ext: &str) -> String` → `"record-<unix_millis>.<ext>"`.
  Task 6 (`screenshot.rs`) and Task 7 (`record.rs`) call `data_dir()` and
  the filename helpers directly.

- [ ] **Step 1: Write `src/paths.rs`**

```rust
//! Output directory + filename generation for captured artifacts.
//! `CAPTURE_PLUGIN_DIR` overrides the default; the directory is created
//! on first use (`fs::create_dir_all`, idempotent).

use std::path::PathBuf;

pub const DIR_ENV: &str = "CAPTURE_PLUGIN_DIR";

/// Resolve (and ensure) the output directory. Never fails the caller on a
/// create error — `fs::create_dir_all`'s `Result` is surfaced by the first
/// actual file write instead, keeping this a pure path-resolution fn.
pub fn data_dir() -> PathBuf {
    let dir = std::env::var(DIR_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn default_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/share/vynkor/capture")
}

pub fn screenshot_filename(ext: &str) -> String {
    format!("screenshot-{}.{ext}", unix_millis())
}

pub fn record_filename(ext: &str) -> String {
    format!("record-{}.{ext}", unix_millis())
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // std::env::set_var races across parallel test threads within this
    // process; serialize the env-mutating tests in this module only.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn data_dir_honors_override_and_creates_it() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("nested/capture");
        std::env::set_var(DIR_ENV, &target);
        let got = data_dir();
        std::env::remove_var(DIR_ENV);
        assert_eq!(got, target);
        assert!(target.is_dir());
    }

    #[test]
    fn filenames_carry_extension_and_prefix() {
        let s = screenshot_filename("png");
        assert!(s.starts_with("screenshot-") && s.ends_with(".png"), "{s}");
        let r = record_filename("mp4");
        assert!(r.starts_with("record-") && r.ends_with(".mp4"), "{r}");
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cd plugins/capture && cargo test paths::`
Expected: PASS (2 tests)

- [ ] **Step 3: Commit**

```bash
git add plugins/capture/src/paths.rs plugins/capture/src/lib.rs
git commit -m "feat(capture): add output data-dir + filename helpers"
```

---

### Task 4: Session/backend detection for `capture_status`

**Files:**
- Create: `plugins/capture/src/session.rs`
- Modify: `plugins/capture/src/lib.rs` — append `pub mod session;`

**Interfaces:**
- Consumes: nothing
- Produces: `pub enum SessionType { Wlroots, Gnome, Kde, X11, Unknown }`
  with `pub fn detect_session(env: &dyn Fn(&str) -> Option<String>) ->
  SessionType` (env lookup injected so tests don't touch the real
  process environment); `pub fn binary_on_path(name: &str) -> bool` (pure
  `$PATH` scan, no spawn); `pub struct StatusReport { pub session_type:
  &'static str, pub screenshot_backend: Option<&'static str>,
  pub record_backend: Option<&'static str>, pub ocr_available: bool,
  pub portal_available: bool }` with `pub fn build_status_report() ->
  StatusReport`. Task 10 wires `capture_status` to call
  `build_status_report()` and serialize it.

- [ ] **Step 1: Write `src/session.rs`**

```rust
//! Session/DE detection (`XDG_SESSION_TYPE`/`XDG_CURRENT_DESKTOP`/
//! `WAYLAND_DISPLAY`/`DISPLAY`) and a pure `$PATH` scan used only for
//! `capture_status` reporting. Actual screenshot/record calls do NOT
//! consult this module — they try their argv chain directly and fall
//! through on `ErrorKind::NotFound` (see `spawner.rs`), so a stale or
//! wrong detection here can never break a real capture, only misreport
//! `capture_status`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    Wlroots,
    Gnome,
    Kde,
    X11,
    Unknown,
}

impl SessionType {
    pub const fn as_str(self) -> &'static str {
        match self {
            SessionType::Wlroots => "wlroots",
            SessionType::Gnome => "gnome",
            SessionType::Kde => "kde",
            SessionType::X11 => "x11",
            SessionType::Unknown => "unknown",
        }
    }
}

pub fn detect_session(env: &dyn Fn(&str) -> Option<String>) -> SessionType {
    let is_wayland = env("WAYLAND_DISPLAY").is_some()
        || env("XDG_SESSION_TYPE").as_deref() == Some("wayland");
    let desktop = env("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();

    if is_wayland {
        if desktop.contains("gnome") {
            return SessionType::Gnome;
        }
        if desktop.contains("kde") {
            return SessionType::Kde;
        }
        // Hyprland/sway/river/labwc all identify as themselves in
        // XDG_CURRENT_DESKTOP, not as a shared "wlroots" string — treat
        // any non-GNOME/KDE Wayland session as the wlroots-protocol family,
        // which is what grim/slurp/wf-recorder actually depend on.
        return SessionType::Wlroots;
    }
    if env("DISPLAY").is_some() {
        return SessionType::X11;
    }
    SessionType::Unknown
}

/// Pure `$PATH` scan — does not spawn anything.
pub fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub session_type: &'static str,
    pub screenshot_backend: Option<&'static str>,
    pub record_backend: Option<&'static str>,
    pub ocr_available: bool,
    pub portal_available: bool,
}

pub fn build_status_report() -> StatusReport {
    let session = detect_session(&|k| std::env::var(k).ok());
    let screenshot_backend = match session {
        SessionType::Wlroots if binary_on_path("grim") => Some("grim"),
        SessionType::Gnome if binary_on_path("gnome-screenshot") => Some("gnome-screenshot"),
        SessionType::Kde if binary_on_path("spectacle") => Some("spectacle"),
        SessionType::X11 if binary_on_path("maim") => Some("maim"),
        SessionType::X11 if binary_on_path("scrot") => Some("scrot"),
        SessionType::X11 if binary_on_path("import") => Some("import"),
        _ => None,
    };
    let record_backend = match session {
        SessionType::Wlroots if binary_on_path("wf-recorder") => Some("wf-recorder"),
        SessionType::X11 if binary_on_path("ffmpeg") => Some("ffmpeg"),
        _ => None,
    };
    StatusReport {
        session_type: session.as_str(),
        screenshot_backend,
        record_backend,
        ocr_available: binary_on_path("tesseract"),
        portal_available: binary_on_path("xdg-desktop-portal") || screenshot_backend.is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn detects_wlroots_on_hyprland() {
        let env = env_map(&[("WAYLAND_DISPLAY", "wayland-1"), ("XDG_CURRENT_DESKTOP", "Hyprland")]);
        assert_eq!(detect_session(&env), SessionType::Wlroots);
    }

    #[test]
    fn detects_gnome_wayland() {
        let env = env_map(&[("WAYLAND_DISPLAY", "wayland-0"), ("XDG_CURRENT_DESKTOP", "GNOME")]);
        assert_eq!(detect_session(&env), SessionType::Gnome);
    }

    #[test]
    fn detects_kde_wayland() {
        let env = env_map(&[("WAYLAND_DISPLAY", "wayland-0"), ("XDG_CURRENT_DESKTOP", "KDE")]);
        assert_eq!(detect_session(&env), SessionType::Kde);
    }

    #[test]
    fn detects_x11_when_no_wayland_display() {
        let env = env_map(&[("DISPLAY", ":0")]);
        assert_eq!(detect_session(&env), SessionType::X11);
    }

    #[test]
    fn unknown_when_nothing_set() {
        let env = env_map(&[]);
        assert_eq!(detect_session(&env), SessionType::Unknown);
    }

    #[test]
    fn binary_on_path_finds_real_binary_and_rejects_bogus_one() {
        // `sh` exists on every POSIX CI runner; a random UUID-ish name won't.
        assert!(binary_on_path("sh"));
        assert!(!binary_on_path("definitely-not-a-real-binary-xyz123"));
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cd plugins/capture && cargo test session::`
Expected: PASS (6 tests)

- [ ] **Step 3: Commit**

```bash
git add plugins/capture/src/session.rs plugins/capture/src/lib.rs
git commit -m "feat(capture): add session/backend detection for capture_status"
```

---

### Task 5: Spawner trait (process execution boundary)

**Files:**
- Create: `plugins/capture/src/spawner.rs`
- Modify: `plugins/capture/src/lib.rs` — append `pub mod spawner;`

**Interfaces:**
- Consumes: nothing
- Produces: `pub trait Spawner: Send + Sync { async fn run_detached(&self,
  bin: &str, args: &[String]) -> Result<BoxedProcess, String>; async fn
  run_capturing(&self, bin: &str, args: &[String]) -> Result<(i32, String),
  String>; }`, `pub trait Process: Send { fn pid(&self) -> Option<u32>; fn
  start_kill(&mut self); fn try_wait(&mut self) -> Option<i32>; }`,
  `pub type BoxedProcess = Box<dyn Process>;`, `pub struct RealSpawner;`,
  and (test-only) `pub struct FakeSpawner`. `run_detached` is for
  long-lived processes (`wf-recorder`, `ffmpeg`) the caller controls the
  lifetime of; `run_capturing` is for short-lived processes whose stdout
  the caller needs (`slurp`, `slop`, `tesseract`). Both return
  `"ERR_CAPTURE_PROVIDER_MISSING: binary '<bin>' not found on PATH"` on
  `ErrorKind::NotFound` so chain code (Task 6/7/8) can recognize it and
  fall through. Tasks 6, 7 (record), and 8 (ocr) all depend on this trait.

- [ ] **Step 1: Write `src/spawner.rs`**

```rust
//! Process execution boundary. Every screenshot/record/OCR backend spawns
//! a host binary directly with argv — never a shell — through this trait,
//! so tests run against [`FakeSpawner`] and never touch a real binary.

use std::io::ErrorKind;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::{Child, Command};

#[async_trait]
pub trait Process: Send {
    fn pid(&self) -> Option<u32>;
    fn start_kill(&mut self);
    fn try_wait(&mut self) -> Option<i32>;
}

pub type BoxedProcess = Box<dyn Process>;

#[async_trait]
pub trait Spawner: Send + Sync {
    /// Spawn `bin args` detached (stdio discarded); caller owns the
    /// returned handle's lifetime (records, long-running captures).
    async fn run_detached(&self, bin: &str, args: &[String]) -> Result<BoxedProcess, String>;

    /// Spawn `bin args`, wait for exit, and capture stdout as UTF-8. Used
    /// for short commands whose output the caller needs (`slurp`, `slop`,
    /// `tesseract ... stdout`).
    async fn run_capturing(&self, bin: &str, args: &[String]) -> Result<(i32, String), String>;
}

fn not_found_or_spawn_failed(bin: &str, e: std::io::Error) -> String {
    if e.kind() == ErrorKind::NotFound {
        format!("ERR_CAPTURE_PROVIDER_MISSING: binary '{bin}' not found on PATH")
    } else {
        format!("ERR_CAPTURE_BACKEND: spawn '{bin}' failed: {e}")
    }
}

pub struct RealSpawner;

#[async_trait]
impl Spawner for RealSpawner {
    async fn run_detached(&self, bin: &str, args: &[String]) -> Result<BoxedProcess, String> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match cmd.spawn() {
            Ok(child) => Ok(Box::new(RealProcess { child })),
            Err(e) => Err(not_found_or_spawn_failed(bin, e)),
        }
    }

    async fn run_capturing(&self, bin: &str, args: &[String]) -> Result<(i32, String), String> {
        let mut cmd = Command::new(bin);
        cmd.args(args).stdin(Stdio::null());
        let output = cmd.output().await.map_err(|e| not_found_or_spawn_failed(bin, e))?;
        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((code, stdout))
    }
}

struct RealProcess {
    child: Child,
}

#[async_trait]
impl Process for RealProcess {
    fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    fn start_kill(&mut self) {
        let _ = self.child.start_kill();
    }

    fn try_wait(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }
}

#[cfg(test)]
pub use fake::FakeSpawner;

#[cfg(test)]
mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    pub struct FakeSpawner {
        /// Per-binary outcome for `run_detached`; unlisted binaries "not found".
        pub detached_ok: StdMutex<HashMap<String, bool>>,
        /// Per-binary `(exit_code, stdout)` for `run_capturing`; unlisted
        /// binaries "not found".
        pub capturing: StdMutex<HashMap<String, (i32, String)>>,
        pub detached_calls: StdMutex<Vec<(String, Vec<String>)>>,
        pub capturing_calls: StdMutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeSpawner {
        pub fn new() -> Self {
            Self {
                detached_ok: StdMutex::new(HashMap::new()),
                capturing: StdMutex::new(HashMap::new()),
                detached_calls: StdMutex::new(Vec::new()),
                capturing_calls: StdMutex::new(Vec::new()),
            }
        }

        pub fn allow_detached(&self, bin: &str) {
            self.detached_ok.lock().unwrap().insert(bin.to_string(), true);
        }

        pub fn set_capturing(&self, bin: &str, code: i32, stdout: &str) {
            self.capturing.lock().unwrap().insert(bin.to_string(), (code, stdout.to_string()));
        }
    }

    struct FakeProcess {
        exited: bool,
    }

    #[async_trait]
    impl Process for FakeProcess {
        fn pid(&self) -> Option<u32> {
            Some(1)
        }
        fn start_kill(&mut self) {
            self.exited = true;
        }
        fn try_wait(&mut self) -> Option<i32> {
            if self.exited {
                Some(0)
            } else {
                None
            }
        }
    }

    #[async_trait]
    impl Spawner for FakeSpawner {
        async fn run_detached(&self, bin: &str, args: &[String]) -> Result<BoxedProcess, String> {
            self.detached_calls.lock().unwrap().push((bin.to_string(), args.to_vec()));
            if self.detached_ok.lock().unwrap().get(bin).copied().unwrap_or(false) {
                Ok(Box::new(FakeProcess { exited: false }))
            } else {
                Err(format!("ERR_CAPTURE_PROVIDER_MISSING: binary '{bin}' not found on PATH"))
            }
        }

        async fn run_capturing(&self, bin: &str, args: &[String]) -> Result<(i32, String), String> {
            self.capturing_calls.lock().unwrap().push((bin.to_string(), args.to_vec()));
            self.capturing
                .lock()
                .unwrap()
                .get(bin)
                .cloned()
                .ok_or_else(|| format!("ERR_CAPTURE_PROVIDER_MISSING: binary '{bin}' not found on PATH"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn real_spawner_reports_missing_binary() {
        let sp = RealSpawner;
        let err = sp.run_detached("definitely-not-a-real-binary-xyz123", &[]).await.unwrap_err();
        assert!(err.contains("ERR_CAPTURE_PROVIDER_MISSING"), "{err}");
    }

    #[tokio::test]
    async fn fake_spawner_falls_through_on_unlisted_binary() {
        let sp = FakeSpawner::new();
        sp.allow_detached("grim");
        let err = sp.run_detached("gnome-screenshot", &[]).await.unwrap_err();
        assert!(err.contains("ERR_CAPTURE_PROVIDER_MISSING"), "{err}");
        assert!(sp.run_detached("grim", &[]).await.is_ok());
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cd plugins/capture && cargo test spawner::`
Expected: PASS (2 tests)

- [ ] **Step 3: Commit**

```bash
git add plugins/capture/src/spawner.rs plugins/capture/src/lib.rs
git commit -m "feat(capture): add Spawner/Process process-execution boundary"
```

---

### Task 6: Screenshot backend chain + `capture_screenshot` handler

**Files:**
- Create: `plugins/capture/src/screenshot.rs`
- Modify: `plugins/capture/src/lib.rs` — append `pub mod screenshot;`

**Interfaces:**
- Consumes: `crate::error::CaptureError`, `crate::spawner::{Spawner,
  FakeSpawner}` (test-only), `crate::session::{detect_session, SessionType,
  binary_on_path}`, `crate::paths::{data_dir, screenshot_filename}`.
- Produces: `pub enum Region { Full, Select, Rect { x: i32, y: i32, w: u32,
  h: u32 } }` with `pub fn parse(v: &serde_json::Value) ->
  Result<Region, CaptureError>` (missing/absent `region` key defaults to
  `Region::Full`); `pub async fn capture_screenshot(spawner: &dyn Spawner,
  params: &serde_json::Value) -> Result<serde_json::Value, CaptureError>`.
  Task 10 wires the `"capture_screenshot"` match arm in `main.rs` to this
  fn, passing `&RealSpawner`. Task 7 (portal) adds one more candidate to
  this module's chain — do not consider this module closed until Task 7
  lands.

- [ ] **Step 1: Write `src/screenshot.rs`**

```rust
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
                    return run_and_finish(spawner, cand.bin, args, ext, path, &path_str).await;
                }
                Ok(_) => return Err(CaptureError::Cancelled("slurp")),
                Err(e) if e.contains("ERR_CAPTURE_PROVIDER_MISSING") => continue,
                Err(e) => return Err(CaptureError::Backend(e)),
            }
        }

        let Some(args) = (cand.build)(region, &path_str) else {
            continue; // this backend can't express the requested region
        };
        return run_and_finish(spawner, cand.bin, args, ext, path, &path_str).await;
    }

    Err(CaptureError::NotSupported("screenshot"))
}

async fn run_and_finish(
    spawner: &dyn Spawner,
    bin: &str,
    args: Vec<String>,
    ext: &str,
    path: std::path::PathBuf,
    path_str: &str,
) -> Result<Value, CaptureError> {
    let (code, _stdout) = spawner
        .run_capturing(bin, &args)
        .await
        .map_err(|e| {
            if e.contains("ERR_CAPTURE_PROVIDER_MISSING") {
                CaptureError::NotSupported("screenshot")
            } else {
                CaptureError::Backend(e)
            }
        })?;
    if code != 0 {
        return Err(CaptureError::Cancelled(bin_static(bin)));
    }
    let (width, height) = image_dimensions_best_effort(&path);
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

fn bin_static(bin: &str) -> &'static str {
    match bin {
        "grim" => "grim",
        "gnome-screenshot" => "gnome-screenshot",
        "spectacle" => "spectacle",
        "maim" => "maim",
        "scrot" => "scrot",
        "import" => "import",
        _ => "screenshot",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn falls_through_from_maim_to_scrot_on_x11() {
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::set_var("DISPLAY", ":0");
        std::env::remove_var("XDG_SESSION_TYPE");
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
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::set_var("DISPLAY", ":0");
        let sp = crate::spawner::FakeSpawner::new();
        sp.set_capturing("maim", 1, ""); // maim found, but -s was Esc'd -> nonzero exit
        let err = capture_screenshot(&sp, &serde_json::json!({"region": "select"})).await.unwrap_err();
        assert!(matches!(err, CaptureError::Cancelled("maim")), "{err:?}");
        std::env::remove_var("DISPLAY");
    }

    #[tokio::test]
    async fn not_supported_when_no_backend_present() {
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("DISPLAY");
        let sp = crate::spawner::FakeSpawner::new();
        let err = capture_screenshot(&sp, &serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, CaptureError::NotSupported("screenshot")));
    }
}
```

**Note for the implementer:** the env-mutating tests in this module
(`falls_through_from_maim_to_scrot_on_x11`,
`cancelled_selection_is_terminal_not_fallthrough`,
`not_supported_when_no_backend_present`) share real process env vars and
must run with `cargo test -- --test-threads=1` for this module, OR be
serialized with the same `static ENV_LOCK: Mutex<()>` pattern used in
Task 3's `paths.rs` tests — add that lock here too before considering the
task done (copy the pattern, don't skip it: flaky env-based tests are
worse than no tests).

- [ ] **Step 2: Add the `ENV_LOCK` mutex and acquire it in each of the three env-mutating tests** (same pattern as Task 3 Step 1's `paths.rs` test module — a `static ENV_LOCK: Mutex<()> = Mutex::new(());` at the top of `mod tests`, `let _g = ENV_LOCK.lock().unwrap();` as the first line of each of those three tests).

- [ ] **Step 3: Run the tests**

Run: `cd plugins/capture && cargo test screenshot:: -- --test-threads=1`
Expected: PASS (11 tests)

- [ ] **Step 4: Commit**

```bash
git add plugins/capture/src/screenshot.rs plugins/capture/src/lib.rs
git commit -m "feat(capture): add screenshot backend chain (grim/gnome/kde/x11)"
```

---

### Task 7: Portal fallback for `capture_screenshot`

**Files:**
- Create: `plugins/capture/src/portal.rs`
- Modify: `plugins/capture/src/screenshot.rs` — `capture_screenshot`'s
  final `Err(CaptureError::NotSupported("screenshot"))` becomes a call
  into `crate::portal::screenshot_via_portal(region, &path_str)` first
  (see Step 3 below).
- Modify: `plugins/capture/src/lib.rs` — append `pub mod portal;`

**Interfaces:**
- Consumes: `crate::error::CaptureError`
- Produces: `pub async fn screenshot_via_portal(interactive: bool, dest:
  &std::path::Path) -> Result<(), CaptureError>` — calls
  `org.freedesktop.portal.Screenshot.Screenshot(parent_window: "",
  options: {"interactive": bool})` over the session bus (same
  `zbus::Connection::session()` + generic `zbus::Proxy` pattern
  `plugins/hotkey/src/portal.rs` already uses for `GlobalShortcuts`),
  waits on the `org.freedesktop.portal.Request` object's `Response`
  signal, extracts `results["uri"]` (a `file://` URI), and copies that
  file to `dest` (the portal writes to its own tmp location, e.g.
  `/run/user/1000/...`, not the caller's chosen path).

- [ ] **Step 1: Write `src/portal.rs`**

```rust
//! `org.freedesktop.portal.Screenshot` fallback — the universal path when
//! no direct binary was found for the session (unknown/future DE,
//! sandboxed environment). Always interactive by construction (the portal
//! shows its own picker), same session-bus stack `hotkey`'s
//! `GlobalShortcuts` backend and `media`'s MPRIS watcher already use.

use zbus::zvariant::{ObjectPath, OwnedValue, Value as ZValue};
use zbus::{Connection, Proxy};

use crate::error::CaptureError;

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENSHOT_INTERFACE: &str = "org.freedesktop.portal.Screenshot";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

pub async fn screenshot_via_portal(interactive: bool, dest: &std::path::Path) -> Result<(), CaptureError> {
    let conn = Connection::session()
        .await
        .map_err(|e| CaptureError::Backend(format!("portal session bus: {e}")))?;
    let proxy = Proxy::new(&conn, PORTAL_SERVICE, PORTAL_PATH, SCREENSHOT_INTERFACE)
        .await
        .map_err(|e| CaptureError::Backend(format!("portal proxy: {e}")))?;

    let mut options: std::collections::HashMap<&str, ZValue> = std::collections::HashMap::new();
    options.insert("interactive", ZValue::from(interactive));
    options.insert("handle_token", ZValue::from("vynkor_capture"));

    let handle: ObjectPath = proxy
        .call("Screenshot", &("", options))
        .await
        .map_err(|e| CaptureError::Backend(format!("Screenshot call: {e}")))?;

    let request = Proxy::new(&conn, PORTAL_SERVICE, handle.as_str(), REQUEST_INTERFACE)
        .await
        .map_err(|e| CaptureError::Backend(format!("request proxy: {e}")))?;
    let mut stream = request
        .receive_signal("Response")
        .await
        .map_err(|e| CaptureError::Backend(format!("subscribe Response: {e}")))?;
    let msg = tokio::time::timeout(std::time::Duration::from_secs(120), stream.next())
        .await
        .map_err(|_| CaptureError::Cancelled("portal"))?
        .ok_or_else(|| CaptureError::Backend("portal Response stream closed".into()))?;
    let body: (u32, std::collections::HashMap<String, OwnedValue>) = msg
        .body()
        .map_err(|e| CaptureError::Backend(format!("decode Response: {e}")))?;
    let (response_code, results) = body;
    if response_code != 0 {
        return Err(CaptureError::Cancelled("portal"));
    }
    let uri: String = results
        .get("uri")
        .and_then(|v| String::try_from(v.clone()).ok())
        .ok_or_else(|| CaptureError::Backend("portal response missing 'uri'".into()))?;
    let src_path = uri
        .strip_prefix("file://")
        .ok_or_else(|| CaptureError::Backend(format!("unexpected portal uri scheme: {uri}")))?;
    std::fs::copy(src_path, dest).map_err(|e| CaptureError::Backend(format!("copy portal output: {e}")))?;
    Ok(())
}
```

- [ ] **Step 2: Add `futures-util` for `StreamExt::next()`**

Edit `plugins/capture/Cargo.toml`, add:

```toml
futures-util = { version = "0.3", default-features = false }
```

and add `use futures_util::StreamExt;` to the top of `portal.rs`.

- [ ] **Step 3: Wire the portal fallback into `screenshot.rs`**

In `plugins/capture/src/screenshot.rs`, replace the function's final line:

```rust
    Err(CaptureError::NotSupported("screenshot"))
}
```

with:

```rust
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
```

- [ ] **Step 4: Write a unit test for the URI-stripping logic** (the only
  part of `portal.rs` testable without a real D-Bus session — same
  "argv/parsing only, no live D-Bus in CI" boundary `hotkey`'s portal
  tests already accept)

Append to `plugins/capture/src/portal.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn file_uri_strips_scheme() {
        let uri = "file:///run/user/1000/xdg-desktop-portal/abc.png";
        assert_eq!(uri.strip_prefix("file://"), Some("/run/user/1000/xdg-desktop-portal/abc.png"));
    }

    #[test]
    fn non_file_uri_has_no_prefix_match() {
        let uri = "http://example.com/x.png";
        assert_eq!(uri.strip_prefix("file://"), None);
    }
}
```

- [ ] **Step 5: Run the tests**

Run: `cd plugins/capture && cargo build && cargo test portal:: screenshot::`
Expected: builds clean (zbus/futures-util compile), portal tests PASS (2),
screenshot tests still PASS (11, unaffected — no backend is ever
`FakeSpawner`-mocked to reach the portal branch, so its behavior there is
exercised only by these two unit tests plus manual verification in
Task 10)

- [ ] **Step 6: Commit**

```bash
git add plugins/capture/src/portal.rs plugins/capture/src/screenshot.rs plugins/capture/src/lib.rs plugins/capture/Cargo.toml
git commit -m "feat(capture): add xdg-desktop-portal Screenshot fallback"
```

---

### Task 8: OCR (`capture_ocr`)

**Files:**
- Create: `plugins/capture/src/ocr.rs`
- Modify: `plugins/capture/src/lib.rs` — append `pub mod ocr;`

**Interfaces:**
- Consumes: `crate::error::CaptureError`, `crate::spawner::Spawner`
- Produces: `pub async fn capture_ocr(spawner: &dyn Spawner, params:
  &serde_json::Value) -> Result<serde_json::Value, CaptureError>`. Task 10
  wires the `"capture_ocr"` match arm to this fn.

- [ ] **Step 1: Write `src/ocr.rs`**

```rust
//! Local OCR via `tesseract`, fully offline, argv-only. Accepts either a
//! path to an already-written image (typically a prior
//! `capture_screenshot` result) or inline base64 (written to a temp file
//! first, since `tesseract`'s CLI takes a file path, not stdin bytes).

use base64::Engine;
use serde_json::Value;

use crate::error::CaptureError;
use crate::spawner::Spawner;

pub async fn capture_ocr(spawner: &dyn Spawner, params: &Value) -> Result<Value, CaptureError> {
    let lang = params.get("lang").and_then(Value::as_str).unwrap_or("eng").to_string();
    let path_param = params.get("path").and_then(Value::as_str);
    let base64_param = params.get("base64").and_then(Value::as_str);

    let (input_path, _temp_guard) = match (path_param, base64_param) {
        (Some(p), None) => (p.to_string(), None),
        (None, Some(b64)) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| CaptureError::BadParams(format!("invalid base64: {e}")))?;
            let tmp = tempfile::NamedTempFile::new()
                .map_err(|e| CaptureError::Backend(format!("temp file: {e}")))?;
            std::fs::write(tmp.path(), &bytes).map_err(|e| CaptureError::Backend(format!("temp file write: {e}")))?;
            let path = tmp.path().to_string_lossy().to_string();
            (path, Some(tmp))
        }
        (Some(_), Some(_)) => {
            return Err(CaptureError::BadParams("provide exactly one of 'path' or 'base64'".into()))
        }
        (None, None) => {
            return Err(CaptureError::BadParams("provide exactly one of 'path' or 'base64'".into()))
        }
    };

    let (code, stdout) = spawner
        .run_capturing("tesseract", &[input_path, "stdout".to_string(), "-l".to_string(), lang])
        .await
        .map_err(|e| {
            if e.contains("ERR_CAPTURE_PROVIDER_MISSING") {
                CaptureError::NotSupported("ocr")
            } else {
                CaptureError::Backend(e)
            }
        })?;
    if code != 0 {
        return Err(CaptureError::Backend(format!("tesseract exited with code {code}")));
    }
    Ok(serde_json::json!({ "text": stdout }))
}
```

- [ ] **Step 2: Add `tempfile` as a regular dependency** (it's currently
  `[dev-dependencies]` only from Task 1)

Edit `plugins/capture/Cargo.toml` — move `tempfile = "3"` from
`[dev-dependencies]` into `[dependencies]` (tests can still use it from
there; `[dev-dependencies]` entries are not visible to non-test code, and
`ocr.rs`'s `NamedTempFile` needs it in the real build too).

- [ ] **Step 3: Write tests**

Append to `plugins/capture/src/ocr.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawner::FakeSpawner;

    #[tokio::test]
    async fn requires_exactly_one_of_path_or_base64() {
        let sp = FakeSpawner::new();
        let err = capture_ocr(&sp, &serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, CaptureError::BadParams(_)));

        let err = capture_ocr(&sp, &serde_json::json!({"path": "/a.png", "base64": "eA=="})).await.unwrap_err();
        assert!(matches!(err, CaptureError::BadParams(_)));
    }

    #[tokio::test]
    async fn not_supported_when_tesseract_missing() {
        let sp = FakeSpawner::new(); // no binaries allowed
        let err = capture_ocr(&sp, &serde_json::json!({"path": "/a.png"})).await.unwrap_err();
        assert!(matches!(err, CaptureError::NotSupported("ocr")));
    }

    #[tokio::test]
    async fn returns_stdout_text_on_success() {
        let sp = FakeSpawner::new();
        sp.set_capturing("tesseract", 0, "hello world");
        let v = capture_ocr(&sp, &serde_json::json!({"path": "/a.png"})).await.unwrap();
        assert_eq!(v["text"], "hello world");
    }

    #[tokio::test]
    async fn base64_input_is_written_to_a_temp_file_before_spawn() {
        let sp = FakeSpawner::new();
        sp.set_capturing("tesseract", 0, "x");
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"not really an image");
        let v = capture_ocr(&sp, &serde_json::json!({"base64": b64})).await.unwrap();
        assert_eq!(v["text"], "x");
        let calls = sp.capturing_calls.lock().unwrap().clone();
        assert_eq!(calls[0].0, "tesseract");
        assert!(std::path::Path::new(&calls[0].1[0]).exists());
    }

    #[tokio::test]
    async fn real_tesseract_extracts_text_from_fixture_if_installed() {
        if crate::session::binary_on_path("tesseract") {
            // Fixture: a 100x30 white PNG with black text "OCR" is out of
            // scope to embed here; this test is a placeholder boundary for
            // a real fixture the implementer adds under
            // `plugins/capture/tests/fixtures/ocr_sample.png` (any small
            // PNG with clear black-on-white text works). Skipped
            // automatically when no fixture is present yet.
            let fixture = "tests/fixtures/ocr_sample.png";
            if !std::path::Path::new(fixture).exists() {
                eprintln!("skipping: {fixture} not present");
                return;
            }
            let sp = crate::spawner::RealSpawner;
            let v = capture_ocr(&sp, &serde_json::json!({"path": fixture})).await.unwrap();
            assert!(!v["text"].as_str().unwrap().trim().is_empty());
        } else {
            eprintln!("skipping: tesseract not on PATH");
        }
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cd plugins/capture && cargo test ocr::`
Expected: PASS (5 tests — the real-tesseract test self-skips via its
internal guard when the fixture file or binary is absent, which is
expected on a fresh checkout; it is not a fixture this plan requires you
to author, only a slot for a future one)

- [ ] **Step 5: Commit**

```bash
git add plugins/capture/src/ocr.rs plugins/capture/src/lib.rs plugins/capture/Cargo.toml
git commit -m "feat(capture): add tesseract-backed capture_ocr"
```

---

### Task 9: Video record chain + start/stop lifecycle

**Files:**
- Create: `plugins/capture/src/record.rs`
- Modify: `plugins/capture/src/lib.rs` — append `pub mod record;`

**Interfaces:**
- Consumes: `crate::error::CaptureError`, `crate::spawner::{Spawner,
  BoxedProcess}`, `crate::session::{detect_session, SessionType}`,
  `crate::paths::{data_dir, record_filename}`, `crate::screenshot::Region`
  (reused for record's `region` param — same shape, same `parse` fn).
- Produces: `pub struct RecordState` (holds at most one active recording:
  `Option<ActiveRecording>` behind a `tokio::sync::Mutex`, since
  start/stop/auto-timeout all need `&mut` access from different call
  sites — main.rs's sequential serve loop plus a spawned timeout task);
  `pub async fn start(state: &RecordState, spawner: Arc<dyn Spawner>,
  params: &serde_json::Value) -> Result<serde_json::Value, CaptureError>`;
  `pub async fn stop(state: &RecordState, params: &serde_json::Value) ->
  Result<serde_json::Value, CaptureError>`. `App` in `main.rs` (Task 10)
  gets a `record_state: RecordState` field constructed once in `App::new`.

- [ ] **Step 1: Write `src/record.rs`**

```rust
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
            let input = if let Region::Rect { x, y, .. } = region {
                format!("{display}+{x},{y}")
            } else {
                display
            };
            args.push("-i".into());
            args.push(input);
            if let Region::Rect { w, h, .. } = region {
                args.push("-video_size".into());
                args.push(format!("{w}x{h}"));
            }
            args.push("-y".into());
            args.push(path.to_string());
            Some(("ffmpeg", args))
        }
        SessionType::Gnome | SessionType::Kde | SessionType::Unknown => None,
    }
}

pub async fn start(
    state: &RecordState,
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
    let Some((bin, args)) = record_args(session, region, "") else {
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
    let args: Vec<String> = args
        .into_iter()
        .map(|a| if a.is_empty() { path_str.clone() } else { a })
        .collect();

    let process = spawner
        .run_detached(bin, &args)
        .await
        .map_err(|e| {
            if e.contains("ERR_CAPTURE_PROVIDER_MISSING") {
                CaptureError::NotSupported("record")
            } else {
                CaptureError::Backend(e)
            }
        })?;

    let id = format!("rec-{}", unix_millis());
    *guard = Some(ActiveRecording { id: id.clone(), path, process, started_at: std::time::Instant::now() });
    drop(guard);

    spawn_auto_stop(state, id.clone(), Duration::from_millis(max_duration_ms));

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

fn spawn_auto_stop(state: &RecordState, id: String, after: Duration) {
    // `RecordState` is always held behind `Arc<App>` by the caller
    // (main.rs); this fn takes `&RecordState` only to read via a raw
    // pointer captured by the spawned task is NOT done — instead main.rs
    // (Task 10) is responsible for calling `spawn_auto_stop` with an
    // `Arc<RecordState>` clone, not a bare reference. See Task 10 Step 2
    // for the exact call site; this fn signature is corrected there to
    // take `Arc<RecordState>`.
    let _ = (state, id, after);
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}
```

**Stop here and fix the `Arc` issue before writing tests** — `spawn_auto_stop`
above is deliberately left as a documented dead end because `start`
signature only has `&RecordState`, but a `tokio::spawn`ed auto-stop task
needs an owned, `'static` handle. Fix it now:

- [ ] **Step 2: Change `RecordState` to always live behind an `Arc` and fix `start`/`spawn_auto_stop`**

Replace the `pub async fn start(...)` signature and its body's tail (from
`spawn_auto_stop(state, id.clone(), ...)` onward), and replace
`spawn_auto_stop` entirely:

```rust
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
```

Delete the now-unused `spawn_auto_stop` fn entirely.

- [ ] **Step 3: Write tests**

Append to `plugins/capture/src/record.rs`:

```rust
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

    #[tokio::test]
    async fn start_rejects_second_recording_while_one_is_active() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("WAYLAND_DISPLAY", "wayland-1");
        std::env::set_var("XDG_CURRENT_DESKTOP", "Hyprland");
        let sp: Arc<dyn Spawner> = Arc::new(FakeSpawner::new());
        if let Some(fake) = (sp.as_ref() as &dyn std::any::Any).downcast_ref::<FakeSpawner>() {
            fake.allow_detached("wf-recorder");
        }
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
```

Note: `start_rejects_second_recording_while_one_is_active`'s
`downcast_ref` dance exists only because `Arc<dyn Spawner>` erases the
concrete type — simplify by having the test construct `FakeSpawner`
directly and wrap it in `Arc::new(fake) as Arc<dyn Spawner>` where a
second, un-erased `Arc<FakeSpawner>` clone is kept alongside for
`.allow_detached(...)` calls. Rewrite that test's setup as:

```rust
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
```

(this replaces the earlier version of the same test — use this one, the
`downcast_ref` version above was scaffolding to show why it's wrong).

- [ ] **Step 4: Run the tests**

Run: `cd plugins/capture && cargo test record:: -- --test-threads=1`
Expected: PASS (6 tests)

- [ ] **Step 5: Commit**

```bash
git add plugins/capture/src/record.rs plugins/capture/src/lib.rs
git commit -m "feat(capture): add video record start/stop with busy-slot + auto-stop"
```

---

### Task 10: Wire everything into `main.rs` + manifest v2 `plugin.json` + docs

**Files:**
- Modify: `plugins/capture/src/main.rs` (the whole file — replace the
  Task 1 stub `App`/`manifest()`/`handle_action_request` with the full
  versions below)
- Create: `plugins/capture/plugin.json`
- Create: `plugins/capture/README.md`
- Create: `plugins/capture/config.example.yaml`

**Interfaces:**
- Consumes: every module built in Tasks 2–9.
- Produces: the finished plugin binary; no further tasks depend on this
  one.

- [ ] **Step 1: Replace `App`, `manifest()`, and `handle_action_request` in `src/main.rs`**

```rust
use std::sync::Arc;

use capture_plugin::error::CaptureError;
use capture_plugin::record::RecordState;
use capture_plugin::spawner::{RealSpawner, Spawner};
use capture_plugin::{ocr, portal as _, record, screenshot, session};

struct App {
    spawner: Arc<dyn Spawner>,
    record_state: Arc<RecordState>,
}

impl App {
    fn new() -> Self {
        Self { spawner: Arc::new(RealSpawner), record_state: Arc::new(RecordState::new()) }
    }
}

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_SCREEN".to_string()],
        actions: vec![
            "capture_screenshot".to_string(),
            "capture_record_start".to_string(),
            "capture_record_stop".to_string(),
            "capture_ocr".to_string(),
            "capture_status".to_string(),
        ],
        ..Default::default()
    }
}
```

(keep the existing `use vynkor_sdk::proto::...` import line from Task 1's
`main.rs` — only `App`, `manifest()`, and the body of
`handle_action_request` change; `serve`, `unix_millis`, and `main` stay as
written in Task 1.)

Replace `handle_action_request`'s body:

```rust
async fn handle_action_request(app: &App, req: ActionRequest) -> ActionResponse {
    let params: Value = match serde_json::from_slice(&req.params_json) {
        Ok(v) => v,
        Err(e) => {
            return ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionError as i32,
                data_json: Vec::new(),
                error: format!("invalid params_json: {e}"),
            };
        }
    };

    let result: Result<Value, CaptureError> = match req.action.as_str() {
        "capture_screenshot" => screenshot::capture_screenshot(app.spawner.as_ref(), &params).await,
        "capture_record_start" => record::start(Arc::clone(&app.record_state), Arc::clone(&app.spawner), &params).await,
        "capture_record_stop" => record::stop(&app.record_state, &params).await,
        "capture_ocr" => ocr::capture_ocr(app.spawner.as_ref(), &params).await,
        "capture_status" => {
            let s = session::build_status_report();
            Ok(serde_json::json!({
                "session_type": s.session_type,
                "screenshot_backend": s.screenshot_backend,
                "record_backend": s.record_backend,
                "ocr_available": s.ocr_available,
                "portal_available": s.portal_available,
            }))
        }
        other => {
            return ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionNotFound as i32,
                data_json: Vec::new(),
                error: format!("unknown action: {other}"),
            };
        }
    };

    match result {
        Ok(data) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionOk as i32,
            data_json: data.to_string().into_bytes(),
            error: String::new(),
        },
        Err(error) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionError as i32,
            data_json: Vec::new(),
            error: error.to_string(),
        },
    }
}
```

And update `main()`:

```rust
#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let app = Arc::new(App::new());
    let client = VynkorClient::connect_from_env().await?;
    serve(client, app).await
}
```

Also update the existing test module's `start_plugin()` helper (Task 1's
`tests` block) — its `let app = Arc::new(App);` becomes `let app =
Arc::new(App::new());` since `App` is no longer a unit struct.

- [ ] **Step 2: Write `plugin.json`**

```json
{
  "plugin_id": "capture",
  "version": "0.1.0",
  "permissions": ["PERMISSION_SCREEN"],
  "kernel_compatibility_range": { "min": "0.1.0", "max": "*" },
  "binary": "capture",
  "events": [],
  "files": ["capture", "plugin.json"],
  "actions": [
    {
      "name": "capture_screenshot",
      "permission": "PERMISSION_SCREEN",
      "input": {
        "type": "object",
        "properties": {
          "region": {
            "description": "'full' (default), 'select' (interactive), or {x,y,w,h}."
          }
        },
        "additionalProperties": false
      },
      "output": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "width": { "type": "integer" },
          "height": { "type": "integer" },
          "format": { "type": "string" }
        }
      }
    },
    {
      "name": "capture_record_start",
      "permission": "PERMISSION_SCREEN",
      "input": {
        "type": "object",
        "properties": {
          "region": { "description": "'full' (default) or {x,y,w,h}." },
          "max_duration_ms": { "type": "integer", "description": "Default 1800000 (30 min)." }
        },
        "additionalProperties": false
      },
      "output": {
        "type": "object",
        "properties": { "recording_id": { "type": "string" } }
      }
    },
    {
      "name": "capture_record_stop",
      "permission": "PERMISSION_SCREEN",
      "input": {
        "type": "object",
        "properties": { "recording_id": { "type": "string" } },
        "required": ["recording_id"],
        "additionalProperties": false
      },
      "output": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "duration_ms": { "type": "integer" }
        }
      }
    },
    {
      "name": "capture_ocr",
      "permission": "PERMISSION_SCREEN",
      "input": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "base64": { "type": "string" },
          "lang": { "type": "string", "description": "Default 'eng'." }
        },
        "additionalProperties": false
      },
      "output": {
        "type": "object",
        "properties": { "text": { "type": "string" } }
      }
    },
    {
      "name": "capture_status",
      "permission": "PERMISSION_SCREEN",
      "input": { "type": "object", "properties": {}, "additionalProperties": true },
      "output": {
        "type": "object",
        "properties": {
          "session_type": { "type": "string" },
          "screenshot_backend": { "type": ["string", "null"] },
          "record_backend": { "type": ["string", "null"] },
          "ocr_available": { "type": "boolean" },
          "portal_available": { "type": "boolean" }
        }
      }
    }
  ]
}
```

- [ ] **Step 3: Write `config.example.yaml`**

```yaml
# capture plugin — no required config; every setting below has a working default.

# Where screenshots/recordings are written. Default: ~/.local/share/vynkor/capture/
# CAPTURE_PLUGIN_DIR: /home/you/.local/share/vynkor/capture
```

- [ ] **Step 4: Write `README.md`**

```markdown
# capture plugin

Screen capture — screenshot, video record, local OCR — for vynkor
plugins. Every backend is a host binary spawned by argv (never a shell);
webcam/V4L2 is out of scope for v1 (needs a new `PERMISSION_CAMERA` in the
kernel).

Single permission: `PERMISSION_SCREEN`.

## Actions

| Action | Params | Result |
|---|---|---|
| `capture_screenshot` | `region?` (`"full"` default \| `"select"` \| `{x,y,w,h}`) | `{path, width, height, format}` |
| `capture_record_start` | `region?`, `max_duration_ms?` (default 1800000) | `{recording_id}` |
| `capture_record_stop` | `recording_id` | `{path, duration_ms}` |
| `capture_ocr` | `path` \| `base64`, `lang?` (default `eng`) | `{text}` |
| `capture_status` | — | `{session_type, screenshot_backend, record_backend, ocr_available, portal_available}` |

## Backend chains

Screenshot (first present wins, per detected session):
`grim`+`slurp` (wlroots) → `gnome-screenshot` (GNOME) → `spectacle` (KDE)
→ `maim`/`scrot`/`import` (X11) → `xdg-desktop-portal` `Screenshot`
(universal fallback, interactive).

Record: `wf-recorder` (wlroots) → `ffmpeg -f x11grab` (X11) → none on
GNOME/KDE Wayland yet (`ERR_CAPTURE_NOT_SUPPORTED: record` — needs a
PipeWire ScreenCast consumer, tracked as a follow-up).

OCR: `tesseract`, fully offline.

## Storage

`CAPTURE_PLUGIN_DIR` (default `~/.local/share/vynkor/capture/`). Every
action writes there and returns an absolute path — no inline base64
output, no `filesystem`-plugin coupling.

## Error taxonomy

`ERR_CAPTURE_BAD_PARAMS`, `ERR_CAPTURE_NOT_SUPPORTED` (chain exhausted,
names the capability), `ERR_CAPTURE_BUSY` (a recording is already
active), `ERR_CAPTURE_CANCELLED` (interactive selection dismissed),
`ERR_CAPTURE_BACKEND` (a detected backend failed at call time).

## Recording lifecycle

One active recording at a time. `max_duration_ms` (default 30 min)
auto-stops via `SIGINT` (the backend flushes a valid container) if
`capture_record_stop` is never called.
```

- [ ] **Step 5: Full test run + manual smoke test**

Run: `cd plugins/capture && cargo test`
Expected: PASS (all tests from Tasks 1–9, ~29 total)

Then, on the Arch/Hyprland dev machine (this repo's actual target
machine — this step is manual, not automated):

```bash
cd plugins/capture && cargo build --release
CAPTURE_PLUGIN_DIR=/tmp/capture-smoke ./target/release/capture &
```

Since there's no running kernel to register against in a standalone
smoke test, instead run the crate's own fake-kernel e2e test with
`--nocapture` to see the registration log line, and separately verify the
real backend chain manually:

```bash
grim /tmp/capture-smoke-test.png && file /tmp/capture-smoke-test.png
slurp && echo "slurp OK, prints a geometry string"
tesseract /tmp/capture-smoke-test.png stdout | head -5
```

Expected: `grim` produces a valid PNG (`file` reports `PNG image data`),
`slurp` prints a `X,Y WxH`-shaped line to stdout, `tesseract` runs without
error (output content doesn't matter for this smoke test — only that the
binary executes and exits 0).

- [ ] **Step 6: Commit**

```bash
git add plugins/capture/src/main.rs plugins/capture/plugin.json plugins/capture/README.md plugins/capture/config.example.yaml
git commit -m "feat(capture): wire actions into serve loop, add manifest + docs"
```

---

## Self-Review Notes

- **Spec coverage:** all 5 actions from the spec are implemented
  (screenshot/record_start/record_stop/ocr/status); permission model
  (`PERMISSION_SCREEN` only) matches; storage model (own data dir, path
  output, no base64 out) matches; error naming
  (`ERR_CAPTURE_NOT_SUPPORTED`/`ERR_CAPTURE_BUSY`) matches; the portal
  fallback and the honest `ERR_CAPTURE_NOT_SUPPORTED: record` gap on
  GNOME/KDE Wayland both match the spec's explicit scope call.
- **Scope deviation flagged inline:** `{"monitor": N}` from the spec's
  region union is dropped (see Global Constraints) — there is no portable
  way to resolve a numeric index to a compositor output name without a
  Hyprland-specific IPC call. `"select"` covers the practical need.
- **Type consistency:** `Region` is defined once in `screenshot.rs` and
  reused by `record.rs` via `use crate::screenshot::{parse as
  parse_region, Region};` — no duplicate region type. `CaptureError` is
  defined once in `error.rs` and is the `Err` type returned by every
  handler fn (`capture_screenshot`, `record::start`, `record::stop`,
  `capture_ocr`), converted to `String` only at the `main.rs` dispatch
  boundary via `.to_string()` (matches `system`'s and `sound`'s pattern of
  keeping typed errors internal and stringifying at the wire boundary).
- **No placeholder tasks** — the one intentionally-deferred fixture
  (`tests/fixtures/ocr_sample.png` in Task 8) is a real, working,
  self-skipping test today, not a TODO; it upgrades automatically the day
  someone drops a fixture file in, with no code change required.
