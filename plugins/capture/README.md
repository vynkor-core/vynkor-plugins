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

## Testing

This crate's `screenshot`/`record` modules each hold their own
env-mutating-test lock; run the full suite with
`cargo test -- --test-threads=1` to avoid a rare cross-module env-var race
between them (each module's own tests already document this internally
for scoped runs).

## Recording lifecycle

One active recording at a time. `max_duration_ms` (default 30 min)
auto-stops via `SIGINT` (the backend flushes a valid container) if
`capture_record_stop` is never called.

## Concurrency

`main.rs` runs `vynkor-sdk`'s concurrent loop (`ConcurrentHandler` +
`serve_concurrent`), not a sequential one, because `capture_screenshot`'s
`xdg-desktop-portal` fallback can block up to 120 seconds on a live D-Bus
call — a sequential loop would stall every other action request and the
kernel's `Ping` for that whole window.
