# `capture` plugin — design (v1: screen only)

Status: approved by user 2026-09-17. Scope: screenshot + screen video
record + local OCR. Webcam/V4L2 is v2, blocked on a new `PERMISSION_CAMERA`
enum landing in the kernel repo (`vynkor-wire`) — out of scope here.

## Why

ROADMAP.md's `capture` row bundles screen+webcam+video+OCR as one XL plugin.
Decomposed per brainstorming: webcam needs a kernel-side permission that
doesn't exist yet; screen capture needs nothing new (`PERMISSION_SCREEN` is
already defined in proto v1.4, unused so far). Ship the unblocked half now.

Primary driver: the user runs Arch + Hyprland (wlroots, `grim`/`slurp`/
`wf-recorder`/`tesseract`/`ffmpeg` already installed) but the plugin must
degrade correctly on other distros/DEs (GNOME, KDE, X11) — same
multi-backend-chain discipline as `sound`/`clipboard`/`system`.

## Permission

Single `PERMISSION_SCREEN` for every action in v1 (no kernel change needed).

## Actions (manifest v2, object-form per action)

| Action | Params | Result |
|---|---|---|
| `capture_screenshot` | `region` (`"full"` \| `{"monitor":N}` \| `"select"` \| `{"x","y","w","h"}`), `format?` (`png`\|`jpg`, default `png`) | `{path, width, height, format}` |
| `capture_record_start` | `region?` (same shape, default `"full"`), `max_duration_ms?` (default 1_800_000 = 30 min), `format?` (default `mp4`) | `{recording_id}` |
| `capture_record_stop` | `recording_id` | `{path, duration_ms}` |
| `capture_ocr` | `path` (file already on disk — from a prior `capture_screenshot`, or any path readable by the process) \| `base64`, `lang?` (default `eng`) | `{text}` |
| `capture_status` | — | `{session_type, screenshot_backend, record_backend, ocr_available, portal_available}` |

No `save_path` param in v1 — every capture writes into the plugin's own
data dir, keeping the plugin filesystem-permission-free (no coupling to
`filesystem`'s allowlist model). Output is always a path, never inline
base64 — screenshots/video are too large for the tiny-payload base64
convention `tts`/`sound` use for short audio clips.

## Storage

`CAPTURE_PLUGIN_DIR` env var, default `~/.local/share/vynkor/capture/`.
Filenames: `screenshot-<unix_millis>.<ext>`, `record-<unix_millis>.<ext>`.
No retention/cleanup policy in v1 (YAGNI — add if disk pressure becomes a
real complaint; `filesystem`'s allowlist model already gives the agent a
read path into this dir if it needs to list/prune later).

## Backend detection (`capture_status` exposes the same result)

Detection runs once at startup (like `system`'s backend probing), cached,
never re-probed per call — a mid-session compositor swap is out of scope.

**Screenshot chain** (first present wins):
1. Wayland + wlroots protocol available → `grim` present → use it.
   Region: `-g "$(slurp)"` for `"select"`, explicit geometry string for
   rect/monitor.
2. Wayland + GNOME (`gnome-screenshot` present) → `gnome-screenshot -f
   <path>` (full), `-a -f <path>` for `"select"` (interactive area).
   No monitor/rect geometry support in this backend — asked-for
   `{"monitor":N}`/rect on GNOME falls back to full-screen with a
   `warning` field in the result naming the limitation (not an error —
   still produced a usable screenshot).
3. Wayland + KDE (`spectacle` present) → `spectacle -b -n -o <path>`
   (full), `-b -n -r -o <path>` (region, interactive).
4. X11 (any DE, `$DISPLAY` set) → `maim <path>` → `scrot <path>` → `import
   -window root <path>`. Region: `slop` for `"select"`, `-g` geometry for
   rect (`maim`/`scrot` both take `-g`).
5. Universal fallback (no direct tool found, portal present) →
   `org.freedesktop.portal.Screenshot` over zbus (same session-bus stack
   `media`/`hotkey` already use) — interactive by construction, works
   under sandboxing and unknown/future DEs.
6. Nothing found → `ERR_CAPTURE_NOT_SUPPORTED: screenshot backend`.

**Record chain:**
1. wlroots (`wf-recorder` present) → `wf-recorder -f <path>` (+ `-g` for
   region).
2. X11 (`ffmpeg` present, `$DISPLAY` set) → `ffmpeg -f x11grab -i
   $DISPLAY[+x,y] -video_size WxH -y <path>`.
3. Else → `ERR_CAPTURE_NOT_SUPPORTED: record backend` (named gap, not
   faked — GNOME/KDE Wayland portal `ScreenCast` requires consuming a
   PipeWire node in-process, a much heavier dependency than an argv
   spawn chain; explicitly deferred, tracked as a v1.1 follow-up in
   ROADMAP.md, not silently pretended to work).

**OCR:** `tesseract` presence checked once; absent → `ERR_CAPTURE_NOT_SUPPORTED:
ocr backend` at call time, not at startup (OCR is optional even when
screenshot works).

All spawns are argv-only, never a shell — same rule as `clipboard`/
`notify`/`sound`.

## Recording lifecycle

Single active recording slot (mirrors `daemon`'s one-busy-slot pattern):
`capture_record_start` while one is already running → `ERR_CAPTURE_BUSY`.
`max_duration_ms` is enforced by a `tokio::time::sleep` race against the
child process; on expiry the plugin sends the backend's stop signal
(`SIGINT` to `wf-recorder`/`ffmpeg`, both flush a valid file on SIGINT) and
finalizes as if `capture_record_stop` had been called, so a forgotten
recording can't run forever or corrupt the output file.

## Error naming

`ERR_CAPTURE_NOT_SUPPORTED: <capability>` (mirrors `system`'s
`ERR_SYS_NOT_SUPPORTED` convention) and `ERR_CAPTURE_BUSY` (mirrors
`daemon`'s `ERR_DAEMON_BUSY`). Both are plain error strings on the action
response, not new proto types.

## Crate layout

New `plugins/capture/` crate, `vynkor-sdk` 0.0.3 line (matches `sound`),
`zbus` 4 (matches `media`/`hotkey`) for the portal fallback only. Modules:
`detect.rs` (backend probing, cached `OnceCell`), `screenshot.rs`,
`record.rs`, `ocr.rs`, `portal.rs` (zbus Screenshot call), `error.rs`,
`handler.rs` (action dispatch), `main.rs` (manifest + serve loop, plain
sequential `Plugin::serve()` — this is a low-volume, spawn-bound plugin
like `tts`/`stt`, not a storage-class hot path, so it does not need the
SDK's `ConcurrentHandler`).

## Testing

- `detect.rs`: unit tests over an injectable "which" lookup (fn pointer or
  trait, same shape as `system::backends`), covering each chain order and
  the not-found terminal case.
- `screenshot.rs`/`record.rs`: unit tests asserting the exact argv built
  per backend/region combination (no real spawn — same style as
  `mic::recorders`' command-building tests).
- `ocr.rs`: real `tesseract` spawn against a small fixture PNG with known
  text, skipped with a stated reason if `tesseract` isn't on the test
  runner's `$PATH`.
- Fake-kernel e2e (`UnixStream::pair`, like `hotkey`/`daemon`): one
  `capture_status` round trip proving the manifest/action-dispatch wiring,
  independent of which backends the CI box actually has.
- No CI coverage for the interactive portal/GNOME/KDE paths (no compositor
  in CI) — covered by argv-construction unit tests only, consistent with
  how `hotkey`'s portal backend is tested today.

## Out of scope (v1)

- Webcam/V4L2 capture (v2, needs `PERMISSION_CAMERA`).
- `save_path` / writing outside the plugin's own data dir.
- Retention/cleanup of old captures.
- GNOME/KDE Wayland video recording (PipeWire portal consumption).
- Window-specific capture by title/class (needs the not-yet-shipped
  `window` plugin for geometry; monitor/rect/select cover the near-term
  need).
