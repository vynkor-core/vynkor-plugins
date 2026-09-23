# media plugin

Local MPRIS media playback control for vynkor plugins. v1 is offline: it
drives `org.mpris.MediaPlayer2.*` players on the session D-Bus (Spotify,
mpv, VLC, browsers, …) and never opens a network connection, so it
declares no permissions.

## Status

Unreleased (feat/media-streams) — local MPRIS via `zbus` session bus plus per-application audio
streams via PipeWire/PulseAudio host tools. 19 actions; MPRIS goes through
`MprisBackend` (`RealBackend` + `MockBackend`), streams through
`CommandRunner` (`RealRunner` + fake in tests). See `CHANGELOG.md` for
history.

**Unreleased** — per-app streams (`media_streams`, `media_stream_volume/mute/move`), active-player resolution replaces the default player, short player names, sandbox-safe D-Bus auth, `media_raise`/`media_quit` actually dispatched.

**v0.1.0** — 10 actions (`play/pause/play_pause/next/prev/stop/seek/volume/status/list`) via `Seek(offset)` only.
**v0.0.2** — bugfix + minor features: `SetPosition` primary for seek, rate/shuffle/loop in `status`, metadata cache, error taxonomy `ERR_MEDIA_*` unified. See `BUGS.md` for fixed vs remaining.
**v0.0.3** — capability guards (`CanPlay`/`CanPause`/`CanGoNext`/`CanGoPrevious`/`CanSeek`/`CanControl` → `ERR_MEDIA_NOT_SUPPORTED`), D-Bus error reclassification (`No such property` is no longer misreported as `PLAYER_VANISHED`), background signal watcher (`PropertiesChanged` + `Seeked` feed the position cache — full BUG-2 fix for compliant players), `media_seek_relative`.

Parsing covers `xesam:title/artist/album`, `mpris:length/trackid/artUrl`, `PlaybackStatus`,
`Volume`, `Position`, `Rate`, `Shuffle`, `LoopStatus` with `ERR_MEDIA_*` error taxonomy.

## D-Bus interfaces used

| Interface | Purpose |
|---|---|
| `org.freedesktop.DBus` | enumerate `org.mpris.MediaPlayer2.*` names on the session bus |
| `org.mpris.MediaPlayer2` | player identity (`Identity`), `Raise`/`Quit` |
| `org.mpris.MediaPlayer2.Player` | playback control (`Play`/`Pause`/`PlayPause`/`Next`/`Previous`/`Stop`/`Seek`/`SetPosition`/`Shuffle`/`LoopStatus`) + `Metadata`/`Volume`/`Position`/`PlaybackStatus`/`Rate` properties |

## Actions

| Action | Params | Result |
|---|---|---|
| `media_play` | `player?`, `uri?` | `{ ok }` |
| `media_pause` | `player?` | `{ ok, players }` — without `player`: every playing player |
| `media_play_pause` | `player?` | `{ ok, playing }` |
| `media_next` | `player?` | `{ ok }` |
| `media_prev` | `player?` | `{ ok }` |
| `media_stop` | `player?` | `{ ok, players }` — without `player`: every playing player |
| `media_seek` | `position_ms` (≥0), `player?` | `{ position_ms }` — tries `SetPosition(trackId, pos)` then `Seek(offset)` fallback, guards `NoTrack` and `CanSeek` |
| `media_seek_relative` | `offset_ms` (signed int), `player?` | `{ position_ms }` — seeks relative to the current position; result clamps at 0 |
| `media_volume` | `level` (0.0–1.0 or 0–100), `player?` | `{ volume }` — int `1` = 1%, float `1.0` = 100% |
| `media_status` | `player?` | `{ player, status, metadata, volume, position_ms, rate, shuffle, loop_status }` — `position_ms` extrapolates via `Rate` when `Position==0` while `Playing`, `metadata` merges from cache on sparse updates |
| `media_list_players` | — | `{ players: [..] }` |
| `media_shuffle` | `enabled` (bool), `player?` | `{ shuffle }` |
| `media_loop` | `mode` (`none`/`track`/`playlist`), `player?` | `{ loop_status }` |
| `media_raise` | `player?` | `{ ok, player }` — focus the player's window |
| `media_quit` | `player?` | `{ ok, player }` — close the player app (risk medium) |
| `media_streams` | `app?` | `{ backend, streams: [{id, app, volume, muted, running, audible, sink, player?, ...}], sinks: [{id, name, description}] }` |
| `media_stream_volume` | `level`, target | `{ ok, streams, player? }` — mixer volume of that app only |
| `media_stream_mute` | `mode` (`on`/`off`/`toggle`), target | `{ ok, streams, player? }` |
| `media_stream_move` | `sink` (id / name / unique part of description), target | `{ ok, streams, sink, player? }` (risk medium) |

Every transport action result also carries `player` (or `players` for
`media_pause`/`media_stop` without `player`). `media_status` adds `stream`
(the player's audio streams, or `null`). Additive — older clients ignore them.

### Player and stream targets

`player` accepts the full bus name or a short one (`spotify`; `firefox`
matches its single `firefox.instance_*`). Stream actions pick their target
by `stream` (id) > `app` (case-insensitive substring of app name/binary) >
`player`; with none, the active player's streams.

### Active player (no `player` given)

1. Exactly one player `Playing` → it.
2. Several `Playing` → the one whose stream is actually audible (running,
   unmuted, volume > 0); still several → `ERR_MEDIA_AMBIGUOUS` with the list.
3. Nothing playing → the most recently paused player (the watcher records
   `PlaybackStatus` changes); a lone player is used as-is.

`media_pause` / `media_stop` without `player` act on **every** playing
player; `media_play_pause` without `player` pauses everything playing, or
resumes per rule 3 when nothing plays.

### Stream ↔ player join

PipeWire's client carries `pipewire.sec.pid`; D-Bus gives the MPRIS owner's
PID (`GetConnectionUnixProcessID`). Browsers play from a child process, so
the stream PID's ancestors are walked via `/proc` — but the kernel sandbox
gives the plugin its own PID namespace, where host PIDs are not visible, so
there (and for `pactl` without PIDs) the fallback is a name match: player
short name contained in the stream's app name or binary.

## Real-world test (0.0.2, 2026-08-20)

Tested on `firefox.instance_1_424` (YouTube `Архитектурная война: Wayland против Х11`) + `mpd` (`кис-кис - падик.mp3`) + `TelegramDesktop`.

- `list_players/status/volume/play_pause` — OK everywhere.
- `mpd seek 30s` via `SetPosition` — OK (`30s→31s` after 400ms), `shuffle/loop` — OK.
- `firefox seek 15s` — still broken upstream: `SetPosition` `Ok` but `Position` stays `0`, `Seek` clamps large offset to `~1s`, `length` appears only after seek (`null→790000000`). Documented as BUG-1 high, requires browser fix.
- `firefox Shuffle/LoopStatus` — correctly `ERR_MEDIA_PLAYER_VANISHED: No such property` (browser doesn't implement).

See `BUGS.md` for full repro steps and `ROADMAP.md` for v1.1/v1.2 plan.

## Config

Config is read from the environment (the kernel's plugin supervisor
translates the `config.yaml` `env:` entry into these before spawning the
plugin).

| Env var | Default | Meaning |
|---|---|---|
| `MEDIA_PLUGIN_PLAYERS` | — (allow all) | comma-separated allowlist of MPRIS player names (`org.mpris.MediaPlayer2.<name>`) this plugin may control |

There is deliberately no default-player setting: which player "the music"
is changes minute to minute, so it is read from live state (see *Active
player* above). `MEDIA_PLUGIN_DEFAULT_PLAYER` (earlier releases) is ignored.

## Host requirements

- Session D-Bus (MPRIS). Works inside the kernel sandbox — see `src/bus.rs`.
- Audio streams: PipeWire (`pw-dump`, `pw-metadata` from `pipewire`, `wpctl`
  from `wireplumber`) or, without PipeWire, PulseAudio's `pactl`. Without
  either, stream actions return `ERR_MEDIA_AUDIO_UNAVAILABLE` and MPRIS
  actions keep working.
