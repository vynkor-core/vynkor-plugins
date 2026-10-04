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
- Kernel on the phone (`transport=local`): the helper inherits the plugin's
  `max_vmem_mb` address-space limit and segfaults under ~1 GiB (measured: 512 MB →
  SIGSEGV, 1024 MB → fine). Set `max_vmem_mb: 2048`, and `sandbox: false` (no Landlock).

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
