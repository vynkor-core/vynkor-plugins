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
