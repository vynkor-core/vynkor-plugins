# `phone` plugin — design (v0.1: transport + camera)

Status: draft for user review 2026-09-29. Scope of this spec: the
`plugins/phone` crate, layers 1–2 (transport, status/setup, photo, stream).
Layers 3–4 (audio, hardware) are outlined so the transport does not have to
change later, but they ship as later minor versions with their own plans.
The second sub-project ("run the vynkor kernel on the phone") is a separate
spec written after this one ships.

## Why

The user's Xiaomi Mi 6 (Ubuntu Touch 24.04, Halium kernel 4.4.153) has a
camera, microphone, speaker, LED, torch, vibrator and battery controls that
no existing plugin reaches. Everything below was verified live on the device
on 2026-09-29 (notes: `~/mi6-camera-terminal.md`):

- Camera works **without any window or screen**: `libcamera.so.1` (libhybris
  camera compat layer, the API `aalcamera` itself uses) driven from
  `python3` + `ctypes`. Trick: `android_camera_set_preview_texture(ctl, 1)`
  with a dummy texture id — without a preview target Camera2Client parks in
  `WAITING_FOR_PREVIEW_WINDOW` and delivers no frames.
- Preview callback delivers NV21 frames in plain memory; the phone has
  `python3` + `cv2` (`~/pylibs`) so JPEG encoding can happen on the phone.
- Autofocus works (`--af video|picture|auto`), front camera works, no
  shutter click (the click only comes from `captureToLocation`).
- Frame rate is bounded by exposure, not by link or CPU: ~12 fps in a dim
  room, ~24–29 fps with light. MJPEG over wifi holds that rate at 640x480,
  1280x720 and 1920x1080; raw NV21 over wifi does not (720p: 3.5 fps).

Primary driver: give the agent eyes (photo/stream from either camera), with
the phone acting as a remote node now and as the host of the kernel later.

## Non-goals (v0.1)

- No kernel change. No `PERMISSION_CAMERA`; the camera uses
  `PERMISSION_SCREEN` (user decision, 2026-09-29), risk `high` +
  `requires_confirmation`, like `capture`.
- No generic "remote device" abstraction, no Android/adb backend. One
  device family (Ubuntu Touch on this phone), two transports. Generalize
  only when a second device exists.
- No daemon on the phone. The only artifact deployed there is one helper
  script in the user's HOME.
- No live viewer inside the plugin (no HTTP/MJPEG server). A human watches
  with `ssh … | ffplay` (README recipe); the agent reads `latest.jpg`.
- No video-frame envelope on the wire (the protocol only has
  `AudioStreamChunk`). Frames are files, like `capture`.
- No fps-range / exposure control: `libcamera.so.1` exposes none. Documented
  as a limit, not worked around.

## Layers and versions

| Layer | Ships in | Actions |
|---|---|---|
| 1. Transport + status | v0.1.0 | `phone_status`, `phone_setup` |
| 2. Camera | v0.1.0 | `phone_photo`, `phone_stream_start`, `phone_stream_stop`, `phone_stream_status` |
| 3. Audio | v0.2.0 | `phone_mic_start`, `phone_mic_stop`, `phone_speak` (outline below) |
| 4. Hardware | v0.3.0 | `phone_led`, `phone_torch`, `phone_vibrate`, `phone_battery` (outline below) |

`plugin.json` declares **only implemented actions**: an action listed in the
manifest but unimplemented is routed by the kernel and then fails, which is
worse than an unknown-action error. ROADMAP.md tracks layers 3–4.

## Permissions

v0.1: `PERMISSION_NETWORK` (ssh transport), `PERMISSION_SCREEN` (camera).
Per-action `permission` in the manifest (enforced on provider and caller):

| Action | Permission | Risk |
|---|---|---|
| `phone_status` | `PERMISSION_NETWORK` | low |
| `phone_setup` | `PERMISSION_NETWORK` | medium (writes one file into the phone's HOME) |
| `phone_photo`, `phone_stream_start` | `PERMISSION_SCREEN` | high, `requires_confirmation` |
| `phone_stream_stop`, `phone_stream_status` | `PERMISSION_SCREEN` | medium / low |

Later layers add `PERMISSION_AUDIO`, `PERMISSION_AUDIO_STREAM`,
`PERMISSION_IPC_SEND`, `PERMISSION_SYSTEM`. Declaring a risk disables the
inference auto-gate (see the agent-catalog notes), so every action carries an
explicit `risk` and a real `description`.

## Architecture

```
plugins/phone/
  Cargo.toml  plugin.json  config.example.yaml  README.md  ROADMAP.md
  helper/hybcam.py            # embedded with include_str!, deployed by phone_setup
  src/
    main.rs        # ConcurrentHandler + serve_concurrent (like capture)
    lib.rs
    error.rs       # ERR_PHONE_* taxonomy
    transport.rs   # Transport trait, SshTransport, LocalTransport, FakeTransport (tests)
    remote.rs      # remote command builder + quoting + env (uid → XDG_RUNTIME_DIR, MIR_SOCKET)
    helper.rs      # embedded script, sha256, deploy, mismatch detection
    framing.rs     # len32 frame splitter over an AsyncRead
    photo.rs  stream.rs  status.rs
    manifest.rs    # action_specs() via vynkor-plugin-manifest
```

`main.rs` uses `ConcurrentHandler`/`serve_concurrent`, not a sequential loop:
`phone_photo` blocks for seconds on a network round trip and a sequential loop
would stall the kernel `Ping` and every other request (same reason as
`capture`). The plugin makes no outbound plugin-to-plugin calls in v0.1, so
the RPC-proxy pattern from `PLUGIN_AUTHORING.md` §1 is not needed.

### Transport

```rust
#[async_trait]
pub trait Transport: Send + Sync {
    /// Run a command to completion; stdout as bytes (binary-safe), stderr as text.
    async fn run(&self, cmd: &RemoteCmd, stdin: Option<&[u8]>, timeout: Duration)
        -> Result<Output, PhoneError>;
    /// Spawn a long-running command; caller reads stdout, kills on drop.
    async fn spawn(&self, cmd: &RemoteCmd) -> Result<Box<dyn Streaming>, PhoneError>;
}
```

This is the same boundary `capture` has in `Spawner`, extended with binary
stdout and stdin (`capture`'s `run_capturing` returns UTF-8 `String`, which
cannot carry JPEG bytes).

- `SshTransport { host }`: `ssh -o BatchMode=yes -o ConnectTimeout=… -o
  ControlMaster=auto -o ControlPersist=60 -o ControlPath=<plugin dir>/cm-%C
  <host> -- <one remote shell string>`. Key auth only; `StrictHostKeyChecking`
  left at the ssh default (no `accept-new`); the host name comes only from
  `PHONE_PLUGIN_SSH_HOST`, validated `[A-Za-z0-9._-]{1,253}` and never
  starts with `-`.
- `LocalTransport`: runs `RemoteCmd` directly with `Command::new(program)
  .args(args)`, no shell, no ssh. Used when the plugin runs on the phone.
- Selected once at startup by `PHONE_PLUGIN_TRANSPORT=ssh|local` (default
  `ssh`). Actions and their results are identical on both, so moving the
  kernel to the phone changes one env var, not the plugin.

`RemoteCmd { env: Vec<(String,String)>, program: String, args: Vec<String> }`
is structured, never a shell string. Only `SshTransport` renders it to a
single string for the remote shell, through one function: single-quote
quoting (`'` → `'\''`) of every token, with a unit test over hostile inputs
(quotes, `;`, `$()`, backticks, newlines, leading `-`). Arguments the plugin
puts in `RemoteCmd` are fixed strings, validated enums, or bounded integers
formatted by the plugin itself. **No caller-supplied text reaches a command
line.**

### Helper script

`helper/hybcam.py` is the verified `hybcam2.py` with two additions:

- `--framing len32`: each frame is a 4-byte big-endian length followed by
  the JPEG. (An SOI/EOI scanner is fragile; length prefix is exact.) Plain
  concatenation stays available for the `ssh | ffplay` recipe.
- `--snap`: run until autofocus has converged (N frames or ≤ 3 s), emit **one**
  frame, disconnect. Used by `phone_photo`.
- Dead-man switch: `--secs` is always set by the plugin (default
  `max_duration`), so a killed ssh never leaves the single camera held. On
  `EPIPE`/`SIGHUP`/`SIGTERM` the helper stops the preview and disconnects.

`phone_setup` writes it to `<PHONE_PLUGIN_REMOTE_DIR>/hybcam-<sha8>.py`
(sha8 of the embedded content) through `ssh 'cat > tmp && mv tmp final'`
(atomic, idempotent, mode 0644). Calls reference the hashed path, so a
plugin upgrade never runs a stale helper: a missing hashed file yields
`ERR_PHONE_HELPER_MISSING: run phone_setup`, not a Python traceback.

### Camera ownership

One camera session at a time on the plugin side (the HAL allows one client).
A second `phone_stream_start` while a stream runs → `ERR_PHONE_BUSY`.
`phone_photo` while a stream runs on the requested camera returns a copy of
the newest frame (`source: "stream"`) instead of contending for the camera;
on the other camera it → `ERR_PHONE_BUSY`. If some other process holds the
camera (e.g. `lomiri-camera-app`), the helper's `connect FAILED` maps to
`ERR_PHONE_CAMERA` with the helper's stderr tail.

## Actions

| Action | Params | Result |
|---|---|---|
| `phone_status` | — | `{transport, host, reachable, helper: "ok"\|"missing"\|"stale", stream_active, latency_ms}` (no per-stream detail; that is `phone_stream_status`) |
| `phone_setup` | — | `{installed, helper_path, sha8, changed}` |
| `phone_photo` | `camera?` (`back`\|`front`, default `back`), `width?`,`height?` (from a fixed list, default 1280x720), `af?` (`video`\|`picture`\|`auto`\|`off`, default `video`), `flash?` (bool, default false), `quality?` (30–95, default 80) | `{path, width, height, format: "jpg", camera, source: "camera"\|"stream", bytes}` |
| `phone_stream_start` | `camera?`, `width?`,`height?`, `fps?` (1–30, default 15), `quality?`, `af?`, `flash?`, `record?` (bool, default false), `max_duration_ms?` (default 300000, cap 1800000) | `{stream_id, latest_path, record_path\|null, camera, width, height}` |
| `phone_stream_stop` | `stream_id?` (omit = the active one) | `{stopped, frames, duration_ms, record_path\|null}` |
| `phone_stream_status` | — | `{active, stream_id, frames, fps_measured, last_frame_age_ms, camera}` |

Supported sizes are a fixed table taken from the device's enumerated preview
sizes (1920x1080, 1280x720, 800x600, 640x480, 320x240, …); anything else →
`ERR_PHONE_BAD_PARAMS`. `flash: true` lights the camera torch through the
API (`android_camera_set_flash_mode` TORCH) — it is the only way to reach
~24 fps in a dim room, at the cost of heat and battery, hence opt-in.

### Storage

`PHONE_PLUGIN_DIR` (default `~/.local/share/vyn/phone/`; same `vyn/` prefix
convention as `capture`). Files: `photo-<unix_millis>.jpg`,
`latest.jpg` (atomically replaced per frame: write `latest.tmp`, `rename`),
`record-<unix_millis>.mjpg` (only with `record: true`, concatenated JPEGs).
Every action returns absolute paths, never inline base64. No retention in
v0.1 (YAGNI); `record` is opt-in and capped by `max_duration_ms` because
MJPEG at 720p is ~1.6 MB/s.

### Data flow

`phone_photo` (ssh): plugin → `SshTransport.run(RemoteCmd{ env: XDG_RUNTIME_DIR,
MIR_SOCKET; program: python3; args: [helper, --cam, back, -W, 1280, -H, 720,
--fmt, jpeg, --q, 80, --af, video, --snap, --framing, len32, --out, -] })` →
stdout = one len32 frame → plugin validates the JPEG magic bytes and length
bound, writes `photo-<ms>.jpg`, returns the path.

`phone_stream_start`: `SshTransport.spawn(…--secs <max_duration>…)`; a
spawned task owns the child, reads frames via `framing.rs`, updates
`latest.jpg`, counters and (if `record`) the record file. `stop` kills the
child (helper's dead-man switch releases the camera even if the kill is
abrupt). Stream state is one `Arc<Mutex<Option<StreamState>>>` in `App`.

### Remote environment

Camera access needs the same env `qmlscene` needed:
`XDG_RUNTIME_DIR=/run/user/<uid>`, `MIR_SOCKET=/run/user/<uid>/mir_socket_trusted`.
`uid` from `PHONE_PLUGIN_REMOTE_UID` (default `32011`, the `phablet` user).
The helper works with these set; whether it works without `MIR_SOCKET` was
not tested, so the plugin always sets both.

## Errors

`ERR_PHONE_BAD_PARAMS`, `ERR_PHONE_UNREACHABLE` (ssh exit 255 / timeout),
`ERR_PHONE_BUSY`, `ERR_PHONE_HELPER_MISSING`, `ERR_PHONE_CAMERA` (helper
reported connect/no-frames failure; message carries a stderr tail, bounded),
`ERR_PHONE_BACKEND` (anything else). Timeouts: `phone_photo` 15 s,
`phone_status` 5 s, `phone_setup` 15 s.

## Configuration (env, `PHONE_PLUGIN_*`, in `plugin.json` `config_schema`)

| Var | Default | Meaning |
|---|---|---|
| `PHONE_PLUGIN_TRANSPORT` | `ssh` | `ssh` or `local` |
| `PHONE_PLUGIN_SSH_HOST` | `mi6` | ssh host alias (from the user's ssh config) |
| `PHONE_PLUGIN_REMOTE_UID` | `32011` | uid on the phone, for `XDG_RUNTIME_DIR`/`MIR_SOCKET` |
| `PHONE_PLUGIN_REMOTE_DIR` | `~/.local/share/vyn-phone` | where the helper lives on the phone |
| `PHONE_PLUGIN_DIR` | `~/.local/share/vyn/phone/` | photos, `latest.jpg`, records, ssh control socket |

### Sandbox note (`config.example.yaml`)

- On the laptop: `ssh` needs `~/.ssh` and network; if Landlock/seccomp blocks
  that, `sandbox: false` for this plugin, with the same explanation `notify`
  gives.
- **On the phone: `sandbox: false`** (user decision, 2026-09-29). The phone's
  kernel (4.4.153-Halium) has no Landlock (`syscall 444` → `ENOSYS`), and
  `fsaccess.rs` fails closed, so `sandbox: true` cannot start there. This is
  recorded here; enforcing or relaxing it in the kernel is sub-project 2.

## Testing

- `FakeTransport` returns canned `Output` / a scripted `Streaming`; no
  network and no real `ssh` in unit tests.
- Unit tests: param validation (every enum and bound), quoting over hostile
  inputs, `framing.rs` (split frames across reads, zero length, oversized
  length, truncated stream), JPEG magic/length validation, helper hash and
  mismatch detection, ownership rules (photo during stream, double start),
  latest.jpg atomic replace, stream stop and dead-man cleanup, `ERR_*`
  mapping from ssh exit codes and helper stderr.
- Fake-kernel integration test per `PLUGIN_AUTHORING.md` §3 (`UnixStream::pair`
  + register handshake first) for the manifest and one photo round trip.
- Live smoke test `#[ignore]` + `PHONE_PLUGIN_LIVE=1`: `phone_status`,
  `phone_setup`, one back photo, one front photo, a 3 s stream. Run by hand
  against the connected phone; the result goes into the PR description.
- Workspace: add `plugins/phone` to the root `Cargo.toml` `members`. Note:
  the root `Cargo.toml`/`Cargo.lock` are currently untracked in this
  working tree, so the workspace registration lands together with whichever
  change tracks them.

## Layer 3 outline — audio (v0.2.0)

`phone_mic_start` streams `parecord --device=source.droid` PCM through the
transport into `AudioStreamChunk{PCM_S16LE}` envelopes to a `target`
(allowlisted by `PHONE_PLUGIN_IPC_TARGETS`, exactly like `mic`).
`phone_speak` plays a file or base64 clip via `paplay --device=sink.primary_output`,
wrapped in `timeout` (paplay hangs after EOF on this device); the output
sink must run at 48000 Hz (`PULSE_MODULES_DROID_EXTRA_CARD_ARGS=rate=48000`,
already in place). When the kernel runs **on** the phone, the existing `mic`
plugin (its chain includes `parec`) and `sound` should work unchanged — to be
verified in sub-project 2; layer 3 then matters for the ssh transport only.

## Layer 4 outline — hardware (v0.3.0)

`phone_led`, `phone_torch`, `phone_vibrate`, `phone_battery`. LED, torch and
vibrator are root-only sysfs writes, so they need `sudo -S -p ''` with the
password on **stdin** from a 0600 file named by `PHONE_PLUGIN_SUDO_PASS_FILE`
(never argv, never logged, never in the manifest). `phone_battery` is
read-only (`capacity`, `status`, `temp`, `input_suspend`); the existing
`charge-limit` user service keeps owning the charge limit. Torch here means
the phone's LED flashlight (`torch-light0/1`), independent of the camera's
`flash` option.

## Decisions made by default (flag if wrong)

1. Frames are files (`latest.jpg`, optional record) — no wire-level video.
2. `phone_photo` during a stream returns the newest stream frame.
3. Camera permission = `PERMISSION_SCREEN` with `high` risk +
   `requires_confirmation`.
4. Helper deployed to the phone's HOME by an explicit `phone_setup`, not
   lazily on first use, so no action silently writes to the phone.
5. Default resolution 1280x720; default `af: video`.
