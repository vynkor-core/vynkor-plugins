# media plugin — known bugs / tech debt

> File intentionally kept inside the plugin tree so future fixes can link commits to entries here. Each entry has `scope`, `repro`, `expected`, `actual`, `root cause` (if known) and `fix idea`. Triaged by severity. Updated for v0.0.2 (2026-08-20 real test, was 0.2.0).

## Fixed on feat/media-streams (unreleased, 2026-09-23)

### FIXED BUG-5 — every D-Bus call fails under the kernel sandbox (critical)

- **Scope:** all MPRIS actions when `plugins.d/media.yaml` has `sandbox: true` (the `vynm install` default).
- **Repro:** `media_list_players` → `ERR_MEDIA_BUS_UNAVAILABLE: D-Bus handshake failed: Exhausted available AUTH mechanisms`.
- **Root cause:** `vynkor/src/plugins/shim.rs` unshares a user namespace with `uid_map "0 <uid> 1"`; zbus 4.4 sends `AUTH EXTERNAL hex(geteuid())` = uid 0, the bus sees peer uid 1000 and rejects.
- **Fix:** `src/bus.rs` — standard connect first, then an empty-identity `AUTH EXTERNAL` handshake (as sd-bus does; the daemon uses peer credentials), `Builder::authenticated_socket`, explicit `Hello`. Verified live under the sandbox.
- **Wider note:** any other zbus plugin running with `sandbox: true` has the same failure.

### FIXED BUG-6 — default player = alphabetically first (high)

- **Repro:** `mpd` paused + `spotify` playing → `media_pause {}` paused `mpd`.
- **Fix:** active-player resolution (`src/resolve.rs`); `MEDIA_PLUGIN_DEFAULT_PLAYER` removed.

### FIXED BUG-7 — `media_raise` / `media_quit` never dispatched (medium)

- Declared in `plugin.json` and the manifest list, missing from `main.rs` dispatch → `unknown action`. Wired.

## Fixed in 0.2.0

### FIXED BUG-3 — `media_play_pause` race reports wrong `playing` (low) — fixed

- **Scope:** `src/mpris.rs:play_pause_with`
- **Repro:** Firefox Playing → `media_play_pause` → returns `{"ok":true,"playing":true}` but `playerctl status` flips to Paused 80ms later.
- **Fix:** 0.2.0 polls `get_playback_status` with backoff 50+100+150ms, returns flipped value if detected. Verified via `real_test` on `firefox.instance_1_424`.
- **Commit:** fix/media-bugs-v1

### FIXED TECH-3 — MPD `NoTrack` handling — fixed

- **Scope:** `seek_with` + `parse_metadata`
- **Fix:** 0.2.0 checks `track_id == "/TrackList/NoTrack"` → `ERR_MEDIA_NO_TRACK` before `SetPosition`. `length` via `parse_length_value` now accepts `i32/u32/...` and metadata merges via cache.

### FIXED TECH-2 — Error taxonomy inconsistent — fixed

- **Before:** `player 'x' Seek failed` without prefix, `list_names` with `ERR_MEDIA_BUS_UNAVAILABLE`.
- **Fix:** 0.2.0 wraps every D-Bus error: `ERR_MEDIA_BUS_UNAVAILABLE` for `Connection::session`, `ERR_MEDIA_PLAYER_VANISHED` for vanished player, `ERR_MEDIA_SEEK_FAILED / NOT_SUPPORTED / BAD_PARAMS` for seek/set. `player`/`uri` validation now `ERR_MEDIA_BAD_PARAMS`.

### FIXED TECH-1 (partial) — Allowlist parsed per call — mitigated

- **Before:** `allowlist()` `env::var` on every `is_allowed` call (2-3× per action).
- **Fix:** 0.2.0 reads once per `resolve_player`/`list_players_with` via `is_allowed_with(&list, ...)`. Full cache with `OnceLock` + `SIGHUP` reload still TODO (see IDEA).

### FIXED — `parse_volume` int `1` → 100% (critical) — fixed

- **Repro:** `{"level":1}` (int 1%) returned `1.0` (100%) because `as_f64()` swallowed `as_u64` branch.
- **Fix:** 0.2.0 checks `is_u64/is_i64` before `as_f64`, int `1` → `0.01`.

### FIXED — `status_with` swallowed `PLAYER_VANISHED` — fixed

- **Before:** `get_playback_status().unwrap_or("Stopped")`, `get_volume().unwrap_or(0.0)` hid vanished player as fake `Stopped`.
- **Fix:** 0.2.0 propagates `ERR_MEDIA_PLAYER_VANISHED` for `PlaybackStatus/Volume/Position`; `Metadata` still `unwrap_or_default` then merged.

### FIXED — `parse_metadata` length only i64/u64 — fixed

- **Fix:** 0.2.0 `parse_length_value` accepts `i64/u64/i32/u32/i16/u16/u8`.

### FIXED — BUG-4 `Metadata` length missing after Seek (low) — fixed

- **Fix:** 0.2.0 `META_CACHE: OnceLock<Mutex<HashMap<String,MediaMetadata>>>` + `merge_metadata_cached` keeps last-known `length/title/track_id` when fresh is sparse. Verified: Firefox `length null → 790000000` after `Seek` now retained.

## Remaining (high/med)

### BUG-1 — Seek on Firefox/YouTube resets to 0 (high) — still reproducible on Firefox 0.2.0

- **Scope:** `src/mpris.rs:seek_with` + `RealBackend::set_position` + `seek_offset`
- **Repro (2026-08-20):** `firefox.instance_1_424` Playing `Архитектурная война: Wayland против Х11` @ `pos 0` → `media_seek {position_ms:15000}` (harness `mpris::seek`) → `media_status` `position_ms 0`, not `15000`. Direct: `busctl call ... Seek x 5000000` → `Position x 1000000` (1s not 5s), `SetPosition o "/org/mpris/MediaPlayer2/firefox" x 10000000` → `Ok` via zbus but `Position` stays `0` (harness debug `pos 26000000 → set_position 5s → pos 26000000` no move). `playerctl position 10` → stays `0.000007`.
- **Expected:** absolute seek to `target_ms`.
- **Actual:** `Position` stays 0 or clamps to ~1s; `Length` flips `null → 790M` after Seek (now cached).
- **Root cause:** Firefox/Chromium MPRIS for HTML5 `<video>` clamps `Seek` large jumps to 0; `SetPosition` synthesizes `trackId /org/mpris/MediaPlayer2/firefox` but validates per element differently — zbus `ObjectPath` correct, but Firefox ignores synthetic path for YouTube.
- **Fix in 0.2.0:** tried `SetPosition` primary with `ObjectPath::try_from`, fallback `Seek(delta)` on `NotSupported`; overflow guard `checked_mul`. Not enough for Firefox.
- **Fix in 0.0.3:** `CanSeek` guard added (`seek_absolute_on`) per the original fix idea; Firefox reports `CanSeek=true`, so upstream behavior is unchanged — documented as an upstream browser bug, wontfix here. Regression tests kept: `seek_uses_set_position_primary`, `seek_fallback`, `seek_no_track`, plus new guard tests.
- **Files:** `src/mpris.rs:set_position`, `seek_absolute_on`.

## Fixed in 0.0.3

### FIXED BUG-2 — `media_status` stale Position 0 (medium) — fully fixed for compliant players

- **Scope:** signal watcher (`spawn_watch_task` / `run_watch`) + `POS_CACHE` + window-capped `extrapolate_position`
- **Fix:** one watcher task per MPRIS player subscribes `org.freedesktop.DBus.Properties.PropertiesChanged` on `Player` and the `Seeked(int64)` signal, feeding `(pos, rate, updated_at)` into `POS_CACHE` via `cache_note`. `extrapolate_position` now trusts a cached sample only within `EXTRAPOLATION_MAX_AGE_MS` (120s) — MPRIS does not require periodic Position updates, so older samples are ignored. Works fully for compliant players (mpd verified pattern); Firefox stays limited by BUG-1 upstream (`raw_pos` never becomes >0 there).
- **Event publication deferred:** `media.state_changed` needs `PERMISSION_EVENT_PUBLISH` + an outbound path from the watcher task; the SDK's simple sequential loop owns `VynkorClient` exclusively (single-reader rule, see `docs/PLUGIN_AUTHORING.md` §1). Deferred until media migrates to the calendar-style select loop (see ROADMAP v1.2). The watcher only ever writes static caches, so it is single-reader-safe today.

### FIXED — capability guards + error reclassification

- `CanPlay`/`CanPause`/`CanGoNext`/`CanGoPrevious`/`CanSeek`/`CanControl` checked before the corresponding calls → `ERR_MEDIA_NOT_SUPPORTED` naming the property. Only an explicit `false` blocks; missing/unreadable capability properties never block (minimal players keep working) — the underlying failure then surfaces through its own taxonomy.
- D-Bus errors reclassified via `classify_dbus`: `No such property`/`UnknownProperty`/`UnknownMethod`/`NotSupported` → `ERR_MEDIA_NOT_SUPPORTED` (was misreported as `ERR_MEDIA_PLAYER_VANISHED`, e.g. firefox Shuffle); `ServiceUnknown`/`NameHasNoOwner` stay VANISHED.

### FIXED — negative/odd metadata values

- `parse_length_value` rejects negative lengths; mixed-type artist arrays (`av` with `Value::Value`-wrapped elements) unwrap correctly via `downcast_ref::<String>()`; wrong-typed scalar fields yield empty/None instead of panicking paths.

## Tech debt / ideas

### TECH-1 full — Allowlist hot-reload

- One-shot `OnceLock` cache + `media_reload_config` action, or parse in `on_init` `Arc<Vec>`.

### IDEA — `media_events` subscription

- Subscribe `Seeked(int64)` + `PropertiesChanged` → `media.seeked`/`media.state_changed`. Requires `PERMISSION_EVENT_PUBLISH`. Partially done in 0.0.3: signals are consumed by the background watcher and feed `POS_CACHE`; publishing events to the bus is deferred to the v1.2 loop migration (single-reader rule).

### IDEA — Extrapolate Rate fully

- Done partially (0.2.0). Need `Rate` double (1.0 normal, 0.0 paused) multiplied on every `Position` delta; already in `extrapolate_position`, now window-capped in 0.0.3.

### DONE in 0.0.3 — `media_seek_relative`, `Can*` guards

- `CanSeek/CanControl/CanPause/CanPlay/CanGoNext/CanGoPrevious` check before calling → `ERR_MEDIA_NOT_SUPPORTED`.
- `media_seek_relative {offset_ms}` — signed offset, clamps at 0, shares the absolute-seek core with `media_seek`.

## Real test log (2026-08-20)

- Players: `firefox.instance_1_424`, `mpd`, `TelegramDesktop`. Firefox YouTube `xesam:title Архитектурная война...`, `trackId /org/mpris/MediaPlayer2/firefox`, `Volume 0.8`, no `Shuffle/LoopStatus`. MPD `trackId /org/mpd/Tracks/3` when Playing, `NoTrack` when Stopped, `length 155520000`, `Shuffle false→true`, `Loop None→Track` via `media_shuffle/media_loop`.
- `mpc seek 10` → `busctl Position x 11000000` after 1s, `playerctl 11.0` OK. `mpris::seek 30s` on mpd from 6s → 31s OK, second `seek 5s` from 30s stayed 31s if sent within 400ms (race, needs `PropertiesChanged` wait).
- `firefox volume 0.3→0.8` OK, `play_pause` OK, `shuffle` on firefox correctly `ERR_MEDIA_PLAYER_VANISHED: No such property`.

## How to reproduce locally

```bash
playerctl -l  # firefox.instance_1_424 + mpd + TelegramDesktop
cargo run --manifest-path plugins/media/Cargo.toml --bin real_test  # removed after test, see git history
busctl --user call org.mpris.MediaPlayer2.firefox.instance_1_424 /org/mpris/MediaPlayer2 org.mpris.MediaPlayer2.Player Pause
busctl --user get-property org.mpris.MediaPlayer2.firefox.instance_1_424 /org/mpris/MediaPlayer2 org.mpris.MediaPlayer2.Player Position
mpc status; mpc seek 10; busctl --user get-property org.mpris.MediaPlayer2.mpd /org/mpris/MediaPlayer2 org.mpris.MediaPlayer2.Player Position
```
