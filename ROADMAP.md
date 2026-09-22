# vynkor-plugins roadmap

> **Naming:** Vynkor is being renamed **vynkor** ("vynkor core") — the
> kernel and all sibling repos, eventually. New code and docs written from
> now on use **vynkor**; "Vynkor" remains only for the historical name or
> in-flight renames. Stable identifiers (`plugin_id` slugs, binary names,
> `*_PLUGIN_*` env vars, permission strings) keep their current spellings —
> they are protocol/config surfaces, not prose. The `vyn` binary stays
> `vyn`. This file itself will be reworded with the rename.

Plugin ideas beyond what's shipped, and the order/dependencies between them.
Each plugin gets its own `plugins/<name>/ROADMAP.md` once work starts (see
`plugins/ai/ROADMAP.md`, `plugins/network/ROADMAP.md` for the pattern) — this
file is the cross-plugin picture only.

## Shipped

| Plugin | Path | Depends on | Notes |
|---|---|---|---|
| `ping-pong-rs` | `examples/ping-pong-rs/` | — | example plugin, no real capability |
| `network` | `plugins/network/` | — | outbound HTTP, `PERMISSION_NETWORK`, SSRF-guarded. v0.4: gzip/brotli/deflate/zstd, `multipart` bodies, `network_stats`, `cache_ttl_ms`, `use_cookies` |
| `ai` | `plugins/ai/` | `network` | LLM chat completion (anthropic/openai-compatible), declares `network` — caller of `network`'s gated `http_request` (T-19) |
| `database` | `plugins/database/` | — | KV/SQL storage primitive, `PERMISSION_STORAGE`, per-caller SQLite file isolation. v0.3: `db_incr`/`db_keys`/`db_append`/`db_patch`, KV TTL (`ttl_ms`), `db.changed` change events |
| `tts` | `plugins/tts/` | `network` (cloud providers) | text-to-speech — local ONNX (sherpa: Kokoro/Piper) in-process + openai/elevenlabs via `network`, declares `network` (caller of gated `http_request`). **D-12:** `tts_speak` streams Opus `AudioStreamChunk`s to a peer (`PERMISSION_AUDIO_STREAM`). Formats batch: `opus`/`aac`/`flac` for openai, `ulaw_8000` for elevenlabs, local mp3 encode (LAME) for sherpa |
| `stt` | `plugins/stt/` | `network` (cloud provider) | speech-to-text — local ONNX (sherpa: zipformer/whisper) in-process + openai audio via `network`, declares `network` (caller of gated `http_request`). **D-12:** `stt_listen_start`/`stt_listen_stop` stream PCM in and publish a `stt_text` event (`PERMISSION_AUDIO_STREAM`, `PERMISSION_EVENT_PUBLISH`) |
| `secrets` | `plugins/secrets/` | — | encrypted credential/API-key vault (`secret_get`/`secret_set`/`secret_delete`/`secret_list`), ChaCha20-Poly1305 per-caller `.vault` files, master key via `SECRETS_PLUGIN_MASTER_KEY`, `PERMISSION_SECRETS` (proto v1.4) |
| `gated-write` | `reference/gated-write/` | — | reference impl of the D-09 confirmation gate: risky file write split into `request_write` (any caller, `requires_confirmation`) + `confirm_write` (allowlisted callers only), writes confined to a data dir |
| `notify` | `plugins/notify/` | — | desktop/system notifications via host binaries — `notify-send` (libnotify), `wall`, `espeak`; argv-only spawn, never a shell (`PERMISSION_NOTIFY`). v0.2: `speak: true` озвучка через `tts`-плагин (`tts_synthesize` + локальный плеер), `silent: true` + inbox (`notify_list`/`notify_mark_read`/`notify_delete`) — скрытые уведомления, которые будущий `agent` сможет просматривать |
| `sync` | `plugins/sync/` | — | host-side sync state primitive (D-13): versioned SQLite KV + `sync_get_snapshot`/`sync_get`/`sync_set`/`sync_del`, publishes `sync.delta` events on every mutation (`PERMISSION_STORAGE`, `PERMISSION_EVENT_PUBLISH`) |
| `sync-client` | `plugins/sync-client/` | `sync` | client-side mirror + heartbeat scheduler (D-13): subscribes to `sync.delta`, pulls `sync_get_snapshot` on (re)connect to catch up, pushes its heartbeat into host state via `sync_set` on a timer (`PERMISSION_SCHEDULER`, `PERMISSION_IPC_SEND`) |
| `notes` | `plugins/notes/` | `database` | note CRUD as a thin schema layer over `database`'s KV (`note:<id>` JSON docs, atomic id counter), publishes `plugin.notes.changed`; callers need no storage permission — `notes` holds it (T-19) |
| `calendar` | `plugins/calendar/` | `database`, `notify` | event CRUD + reminders: opt-in `remind_before_ms`, timer scan fires once at-most (`late` flag after downtime), publishes `plugin.calendar.changed`/`.due`, best-effort `notify_send`; rescheduling resets the fired flag |
| `media` | `plugins/media/` | — | local MPRIS playback control (`play/pause/next/prev/stop/seek/seek_relative/volume/status/list_players/shuffle/loop`), capability guards (`CanPlay`/…→`ERR_MEDIA_NOT_SUPPORTED`), background `PropertiesChanged`/`Seeked` watcher feeding the position cache; zbus session bus, `permissions: []`. v0.0.3: 13 actions, 42 tests |
| `clipboard` | `plugins/clipboard/` | — | text clipboard read/write via host binaries — `wl-paste`/`wl-copy` (Wayland), `xclip`/`xsel` (X11); argv-only spawn, never a shell; size cap + per-spawn timeout (`PERMISSION_CLIPBOARD`, proto v1.4) |
| `filesystem` | `plugins/filesystem/` | — | sandboxed file read/write + read-only browse (`fs_list`/`fs_read`/`fs_write`) behind a default-deny allowlist of absolute roots (`FILES_PLUGIN_ALLOWED_ROOTS`); deepest-existing-ancestor canonicalize blocks `..` traversal and symlink escapes; write refuses symlink/dir/root targets (`PERMISSION_FILES_READ`/`WRITE`). v0.1.0: 33 tests |
| `search` | `plugins/search/` | `network`, `secrets` | web search (`web_search`) via brave/tavily adapters routed through `network`'s gated `http_request` (T-19 caller), keys vault-first via `secrets`, `SEARCH_PLUGIN_ALLOWED_KEY_ENVS` default-deny allowlist; normalized `{query, results[{title,url,snippet}]}`. v0.1.0: 34 tests incl. fake-kernel end-to-end |
| `system` | `plugins/system/` | — | local host queries + reversible controls — `sys_info`/`sys_battery`/`sys_procs`/`sys_volume[_set/_mute]`/`sys_brightness[_set]`/`sys_lock`/`sys_power_profile[_set]` behind one `PERMISSION_SYSTEM`; backend traits + startup detection, absent capability → `ERR_SYS_NOT_SUPPORTED` naming it. Linux: UPower DisplayDevice, wpctl→pactl, sysfs write→brightnessctl fallback (0 clamps to non-blanking step), ScreenSaver→loginctl chain, ppd both name/path generations; macOS: pmset/osascript/CGSession via non-gated pure parsers. v0.3.0: 45 unit + 4 fake-kernel e2e tests |
| `scheduler` | `plugins/scheduler/` | `database` | once/cron schedules persisted as `sched:<id>` JSON docs over `database` (`PERMISSION_STORAGE`, `PERMISSION_EVENT_PUBLISH`). Scan loop like calendar's (`SCHEDULER_PLUGIN_SCAN_SECS`, first tick = startup catch-up): one-shots resolve `delay_ms` at set time and mark fired BEFORE dispatch (at-most-once, `late: true` after downtime); cron (5/6-field, fixed UTC offset) anchors to its last scheduled fire — missed occurrences skipped. Fire kinds: best-effort `plugin.scheduler.fired` event or kernel-routed action call (gated targets fail into `last_error` unless the operator grants the permission — T-19 holds, no laundering) |
| `sound` | `plugins/sound/` | — | audio output primitive — the single owner of the speakers: `sound_play` (file or inline base64 + format, volume/device) spawns a host player argv-only and returns immediately; `sound_stop` (idempotent, by id or all) / `sound_status`. Chains: wav → `pw-cat --playback` → `paplay` → `aplay`, non-wav → `ffplay`; capability-aware auto filtering (volume drops aplay, device drops ffplay), operator pin via `SOUND_PLUGIN_PLAYER`; replace-on-play; inline audio via capped temp files cleaned on reap; lazy reap, no watcher task. Fully offline (`PERMISSION_AUDIO`, existing enum 5). `notify`'s `speak:true` player migrates here later (see `plugins/sound/ROADMAP.md`) |
| `vector-db` | `plugins/vector-db/` | — | embedding upsert/similarity search (`vec_upsert`/`vec_upsert_batch`/`vec_query`/`vec_get`/`vec_delete`/`vec_list`/`vec_stats`), per-caller SQLite, brute-force cosine, L2-norm, Ollama via `ai` (`nomic-embed-text` 768, `all-minilm:33m` 384), `PERMISSION_STORAGE`+`EVENT_PUBLISH`. v0.1.0: 29 tests incl. deadlock, batch, fallback `error` (см. `plugins/vector-db/README.md`, `USAGE.md`) |
| `daemon` | `plugins/daemon/` | `agent`, `mic`, `stt`, `tts`, `sound` (+`hotkey` for ptt) | headless always-on voice client — the first thin client to `agent` (Kairo): `daemon_turn` runs one listen→think→speak cycle (`stt_listen_start` + `mic_start{target:"stt"}` → endpoint per mode → `mic_stop` flushes EOS → transcript → `goal_start` → `tts_synthesize` → `sound_play`); three listen modes (`window` fixed capture / `vad` ends on stt's speech-ended event — hands-free conversation, needs `STT_PLUGIN_VAD=on` / `ptt` hold-the-hotkey via the hotkey plugin, stuck-key auto-release), `daemon_say`/`daemon_ask` speak without listening; opt-in background loop (`daemon_enable`, off at boot), single busy slot (`ERR_DAEMON_BUSY` on overlap — the mic has one owner), publishes `plugin.daemon.turn.completed`/`.state.changed`, subscribes to stt/hotkey events. Holds only caller permissions (T-19: gated `mic_start`/`sound_play` → `PERMISSION_AUDIO`; publishes need `PERMISSION_EVENT_PUBLISH`). v0.2.0: 29 tests incl. fake-kernel e2e with event injection (см. `plugins/daemon/README.md`, `USAGE.md`) |
| `hotkey` | `plugins/hotkey/` | `mic`/`stt`/`daemon` consumers subscribe to its events | global key combos → kernel events, the push-to-talk source for the daemon's ptt mode: `hotkey_bind`/`hotkey_unbind`/`hotkey_list`/`hotkey_status` + `hotkey_inject` (manual backend event source); operator-spelling triggers (`Ctrl+Shift+Space`) normalize to XDG portal form at bind time; backends `portal` (XDG GlobalShortcuts over zbus 4, same session-bus stack as `media`, Activated/Deactivated = press/release, live rebind per bind/unbind) and `manual` (no OS integration — compositor exec binds/tests call `hotkey_inject`); publishes `plugin.hotkey.hotkey_pressed/.hotkey_released` `{"binding": id}`. No evdev reading (keylogger surface). v0.1.0: 16 tests incl. fake-kernel e2e over UnixStream::pair (см. `plugins/hotkey/README.md`, `USAGE.md`) |

| `automations` | `plugins/automations/` | `database` | declarative `trigger(event) → [conditions] → action` rules engine (CAP-01); cron pairing via subscribing to `plugin.scheduler.fired`; confirmation-hold semantics like gated-write's D-09 spirit (`PERMISSION_STORAGE`, `PERMISSION_EVENT_PUBLISH`) |
| `speech` | `plugins/speech/` | — | `tts`+`stt` merged into one binary (MRG-01) via verbatim path-dep sub-crates; identical actions/events/env. Deployed 2026-09-19: `speech` is now the *installed* registration on this machine — standalone `stt`/`tts` were disabled (`plugins.d/{stt,tts}.yaml.disabled`, binaries backed up, not deleted) because all three declare the same action names (`stt_transcribe`, `tts_speak`, ...) and the kernel's behavior under a duplicate-action-name registration is unverified. Open bug (env-level, not merge-specific — standalone `tts` reproduces identically): local sherpa inference never completes under the kernel supervisor (livelock in `futex_do_wait` after model load); repro matrix ruled out sandbox/RLIMIT_AS/thread-count/cwd/pipe-backpressure/seccomp. Workaround: cloud providers (openai/elevenlabs) unaffected; local engine only on a dev kernel without the resource-capping `pre_exec`. See `plugins/speech/ROADMAP.md` |
| `weather` | `plugins/weather/` | `network` | Open-Meteo current + forecast (INT-08): `weather_now`/`weather_forecast` via `network` gated `http_request`, no key, `PERMISSION_NETWORK`, 9 tests |
| `contacts` | `plugins/contacts/` | `database` | Contact store (INT-12): `contact_create/get/list/update/delete` thin schema `contact:<id>` over `database`, `PERMISSION_STORAGE`+`EVENT_PUBLISH`, 13 tests |
| `tasks` | `plugins/tasks/` | `database` | Task store (INT-19): `task_create/get/list/update/done/delete` thin schema `task:<id>`, `PERMISSION_STORAGE`+`EVENT_PUBLISH`, 10 tests |
| `metrics` | `plugins/metrics/` | — | Host metrics (CAP-03): periodic `/proc`/`statvfs`/`power_supply` samples into `database`, `metrics_query/latest/stats/status` + `sample` events, `PERMISSION_STORAGE` |
| `uptime` | `plugins/uptime/` | `database`, `network` | Health monitor (INT-15): `uptime_add/remove/list/check/history/status` periodic `http_request` checks, `PERMISSION_STORAGE`+`NETWORK` |
| `rss` | `plugins/rss/` | `database`, `network` | Feed reader (INT-05): `rss_add/remove/list/fetch/fetch_all/articles/mark_read/status` `feed:<id>`/`article:<id>` over `database`, dedup by link, `PERMISSION_STORAGE`+`NETWORK` |
| `library` | `plugins/library/` | `database` | Media library index (CAP-09): `library_scan/search/get/random/recent/stats` WalkDir `library:<id>` over `database`, allowlist `LIBRARY_PLUGIN_ALLOWED_ROOTS`, `PERMISSION_STORAGE`+`FILES_READ` |
| `github` | `plugins/github/` | `network`, `secrets` | GitHub outbound (INT-06): `gh_list_issues/gh_create_issue/gh_list_prs/gh_list_runs` via `network` `http_request`, vault-first PAT `GITHUB_PLUGIN_ALLOWED_PAT_ENVS`, `PERMISSION_NETWORK`+`SECRETS` |
| `email` | `plugins/email/` | `secrets` (mailbox creds) | SMTP send + IMAP list (v0.1.0, installed): `email_send`/`email_list` over its own `lettre` SMTP socket — **not** routed through `network`'s `http_request` (SMTP ≠ HTTP), so `sandbox: false`, no `PERMISSION_NETWORK`. Vault-first creds via `secrets`' `secret_get` (`PERMISSION_SECRETS`, T-19), `EMAIL_PLUGIN_ALLOWED_CRED_ENVS` required default-deny allowlist. `email_list` IMAP path is wired (imap+native-tls) but ships stubbable (`EMAIL_PLUGIN_SMTP_STUB`/`IMAP_STUB`). v0.2 open: attachments, CC/BCC/Reply-To, outbox log, retry/backoff, HTTP-provider abstraction |

## Planned

Dependency order — each row can start once everything in "depends on" ships.

| Plugin | Purpose | Depends on | Permissions |
|---|---|---|---|
| `launcher` | launch apps/games by name — Steam (`steam://rungameid/<id>`, reads `libraryfolders.vdf`/`appmanifest_*.acf`) as one provider, generic app launch as another | `filesystem` (read manifests) | `PERMISSION_LAUNCH` (defined, proto v1.4) |
| `capture` | screen/window capture + webcam frame + video recording — one PipeWire-based plugin (screen portal / V4L2 share the stack); OCR is a local tesseract spawn over its own frames (argv-only like `clipboard`/`notify`, fully offline; cloud-vision OCR stays available via `ai` vision for hard cases). Absorbs the old standalone `screenshot` row and the `camera` idea | — | `PERMISSION_SCREEN` (defined, proto v1.4), `PERMISSION_CAMERA` (new, enum 23) |
| `mic` | microphone capture primitive — single owner of the mic, mirror of `sound` (speakers). **Shipped** (v0.1.0, see `plugins/mic/`) — `mic_start`/`mic_stop`/`mic_status` run a capture loop streaming PCM frames out as `AudioStreamChunk`s (D-12 machinery, `tts_speak` in reverse); host-binary record chain `pw-cat --record`→`parec`→`arecord`, argv-only spawn, device/format/rate params; exists so headless clients (`daemon`) get one mic owner instead of each reimplementing capture | — | `PERMISSION_AUDIO`, `PERMISSION_AUDIO_STREAM`, `PERMISSION_IPC_SEND` (peer unicast needs the D-12 gates, same as tts/stt) |
| `wifi` | Wi-Fi control via NetworkManager D-Bus: scan/list/connect/disconnect/forget/toggle radio, known-network management. Credentials stay in NM's own profile store — the plugin orchestrates and never handles raw PSKs | — | `PERMISSION_WIFI` (new, enum 20) |
| `bluetooth` | device list/scan/pair/connect/disconnect, battery level, audio-profile select — BlueZ over D-Bus via zbus (same stack as `media`) | — | `PERMISSION_BLUETOOTH` (new, enum 21) |
| `input` | virtual keyboard/mouse injection — type/click/move/scroll/key-tap via `ydotool`/`wtype`/`xdotool` spawn (argv-only); the agent's hands next to `window` (focus) and `capture` (eyes) | — | `PERMISSION_INPUT` (new, enum 22) |
| `hotkey` | global key combos → kernel events. **Shipped** (v0.1.0, see the Shipped table) — portal + manual backends and runtime bind/unbind as described; still open here: a dedicated `PERMISSION_HOTKEY` (new, enum 24 — must land in the same wire bump as 20–23, installer-probe gap rule; meanwhile holds `PERMISSION_SYSTEM` + `PERMISSION_EVENT_PUBLISH`) and an X11 `XGrabKey` fallback for X11 sessions (compositor/inject wiring covers them meanwhile) | — | `PERMISSION_SYSTEM` (existing) + `PERMISSION_EVENT_PUBLISH` until the wire bump |
| `metrics` | periodic host samples (CPU/RAM/disk/battery/network) into own SQLite + range/query API for webclient graphs; timer loop like calendar's reminder scan, own storage file like vector-db's backend | — | `PERMISSION_STORAGE` (existing) |
| `window` | list/focus/switch/minimize/maximize open windows | — | `PERMISSION_SYSTEM` (existing, shares scope with `system`) |
| `home` | home automation over a custom protocol to bare-metal devices (ESP32/Arduino) — not Home Assistant/MQTT, own wire format | `network` (or serial/BLE transport, TBD) | `PERMISSION_HOME` (defined, proto v1.4) |
| `browser` | read/control active browser tab (url/title/DOM/screenshot) — native-messaging host (the actual plugin, built on `vynkor-sdk-rust`) + a browser extension (Chrome/Firefox) as the tab-access side | — | `PERMISSION_BROWSER` (existing, unused today) |
| `cloud-sync` | remote snapshot transport for D-13 `sync` state — S3/WebDAV/rsync.net via `network`'s gated `http_request`; the host↔remote leg that `sync`/`sync-client` leave host-local (timer pull/push, conflict wins host) | `sync`, `network`, `secrets` (remote creds) | `PERMISSION_NETWORK` (caller of gated `http_request`), `PERMISSION_SECRETS` |
| `agent` | `plugins/agent/` | — | multi-step goal loop: `ai` chat + tool-call dispatch to other plugins' actions, state persisted. **Shipped** (v0.1.0, see the Shipped table) — tool discovery runs over new kernel read-only commands (`list_plugins` + `get_manifest` exemption from `PERMISSION_KERNEL_ADMIN`, see "Kernel-side changes needed") | `ai`, `database` | storage, event_publish (+ operator JWT grant for dispatched actions) |
| `webclient` | browser chat UI + mic voice input/TTS playback, talks to kernel WS API | `agent` (Kairo), `stt`, `tts` | none itself — client only, auth via kernel JWT |
| `daemon` | headless background service: mic listen loop, TTS output, no window/browser. **Shipped** (v0.2.0, see `plugins/daemon/` and the Shipped table) — thin orchestration client: every stage is a kernel-routed action into `mic`/`stt`/`agent`/`tts`/`sound`, the daemon owns only the turn loop and its on/off state; continuous-conversation (`vad`) and push-to-talk (`ptt`) modes make it "walk & talk" capable | `agent` (Kairo), `stt`, `tts`, `mic` (PCM source — nothing else owns the mic headlessly), `sound` (playback) | caller-side only — `PERMISSION_AUDIO` (T-19 for gated `mic_start`/`sound_play`) + `PERMISSION_EVENT_PUBLISH`; auth via kernel JWT |
| `telegram` | third client: two-way chat + voice notes via Telegram bot API | `agent` (Kairo), `stt`, `tts`, `secrets` (bot token) | none itself — client only |

`notes` and `calendar` shipped as exactly that — thin schema + validation on
top of `database`, same relationship `ai` has to `network`. Calendar's v1
reminder scan is an internal timer loop; migrating firing to the future
`scheduler` plugin stays an open option (`plugins/calendar/ROADMAP.md`).

Under the Manifest v2 data-driven permission model (§3), any plugin that
invokes `network`'s gated `http_request` — the shipped
`ai`/`tts`/`stt`/`search` and the planned `email` — declares
`PERMISSION_NETWORK` itself: T-19 requires the *caller* of a gated action to
hold its permission,
and the per-action `permission` in `network`'s manifest makes that check
data-driven ("any caller without the permission is denied").

`secrets` shipped (0.1.0) — `ai`/`tts`/`stt` now resolve provider API keys
**vault-first**: `secret_get` against their own per-caller vault keyed by the
env-var-style handle (`api_key_env`), with the plaintext config env var as
fallback — the vault wins when both exist. `network` keeps no keys (it is the
transport; callers attach them). The allowlist
(`AI_PLUGIN_ALLOWED_KEY_ENVS` etc.) still gates which handles a caller may
reference.

`agent` ships last: it's the integration point for everything else, so it's
the plugin most likely to change shape once the others exist and their real
action surfaces are known.

`webclient`/`daemon`/`telegram` are all thin clients to `agent` — no business
logic of their own, just UI surface (browser, headless mic/speaker, bot chat)
over the same kernel WS API. Separate plugins because their lifecycle
differs: `webclient` opened on demand, `daemon` runs always-on in
background, `telegram` is driven by bot API polling/webhook — different
supervisor/resource-limit config per README's "separate processes" model.
`telegram` is a client, not a `notify` channel — it's two-way (replies,
voice notes in), `notify` stays one-way alert delivery only.

**Sketched, unnamed:** a fourth client that gives `agent` (Kairo) a visible
body — an animated on-screen companion. A small always-on-top Wayland
creature rendered on a `wlr-layer-shell` overlay surface (sprite rig,
data-driven JSON animation clips): speech bubbles, emotes, idle behavior,
reactions to system state; user interaction flows back through it (text
prompt, click/drag). Like the other clients it talks to `agent`, but
unlike them it also *embodies* it: body control (`say`/`emote`/`walk_to`/
`vanish`/`sleep` — exact action names TBD) is served as ordinary manifest
actions, so the agent drives its own avatar through the standard tool-call
loop instead of prompt-side stage directions, and `telegram`/`webclient`
can puppet the body too. Senses and effectors are never reimplemented
in-process — voice is `stt`/`tts`, attention is `window` + `capture`,
media is `media`, app launching is `launcher` — the same split earlier
X11-hack prototypes did by hand, now behind permissions. Implementation
sketch: a `vynkor-sdk-rust` plugin process hosting the Wayland renderer
itself (software sprite rasterizer; precedent for touching the desktop
session from a plugin process is `media` on session D-Bus); if supervisor
coupling to a GUI-bearing process proves annoying, split into a headless
body plugin plus a thin renderer client later. Works unmodified on any
layer-shell compositor (Hyprland, niri, sway/wlroots); compositor-specific
bits stay out of the protocol — global hotkeys are compositor binds
calling a local ctl socket, not a plugin capability. Depends on `ai` at
MVP (chat without the goal loop), upgrades to `agent` when it ships; the
full experience wants `stt`/`tts`, then `window`/`capture`. Permissions:
none of its own beyond what it calls, same model as the other clients.
No table row or directory until the name is decided.

Considered and skipped: `contacts` (fold into `database` as a schema
convention, not its own CRUD/permission), `translate` (`ai` chat completion
already does this via prompt, no dedicated plugin needed), `image`
(image *generation* and *vision* are just more provider calls — vision is
`chat_completion` with base64 image content blocks, same providers, same
vault-first keys, same gated `http_request` routing — so both move into
`ai`'s plan, see `plugins/ai/ROADMAP.md`; local OCR belongs to whoever
holds the pixels, so `capture` runs tesseract over its own frames,
argv-only like `clipboard`/`notify`. A standalone OCR-of-arbitrary-images
plugin stays YAGNI until a real consumer exists — e.g. an agent reading
photos/receipts — and would then be a tiny tesseract-spawn plugin, not a
network+secrets one), `sms` (external
per-message cost for uncertain payoff — `telegram`/`notify` cover the
notification-to-phone case already), `shell` (arbitrary command exec breaks
the narrow-permission-per-plugin model every other plugin follows —
`filesystem`'s read-only `fs_list`/`fs_read` actions cover the "just let it
browse files" use case without an exec surface).

`home` is deliberately not Home Assistant/MQTT — custom wire protocol
talking directly to ESP32/Arduino-class devices, so transport (serial/BLE/
raw socket) needs deciding before real design starts.

`browser` is an extension, not CDP-driven — works against the user's real
browser/profile/logged-in sessions, no `--remote-debugging-port` launch
flag, permissions surfaced through the browser's own extension-permission
UI. Extensions can't open a UDS socket directly, so the plugin has two
halves: a native-messaging host (stdio, spawned by the browser, this is the
real `vynkor-sdk-rust` plugin talking to the kernel) and the extension
itself (JS, `tabs`/`scripting` permissions) relaying over
`chrome.runtime.connectNative`.

## Concurrency model for hot-path plugins

**Shipped** (SDK `vynkor-sdk` 0.1.4, `feat/concurrent-serve-loop`).

The kernel protocol already supports multiple in-flight `ActionRequest`s per
plugin connection — `action_id` is the correlation key end-to-end (see
`ActionRequest`/`ActionResponse` in `wire/proto/vynkor_protocol.proto`), the
pending-action registry tracks them independently
(`src/ipc/protocol.rs:568-577`), and there's already a per-caller concurrency
cap (R6-03). Responses do not need to come back in request order. No kernel
or wire-protocol change was ever needed — this section is now purely a
plugin-implementation pattern, and the pattern lives in the SDK.

The SDK's default `Plugin::serve()` loop is sequential
(`recv().await` → `on_message().await` → reply → next `recv()`), which is
fine for low-volume, network-bound plugins (`ai`, `tts`, `stt`) but wrong
for storage-class plugins called far more often. The SDK now ships a
concurrent message loop as a first-class facility (`vynkor-sdk/src/concurrent.rs`,
replacing the hand-rolled copies that `database` and `network` used to each
maintain):

- `ConcurrentHandler` trait (`id`/`version`/`manifest`/`on_init`/`accept`/
  `on_action`/`on_event`/`on_message`/`on_shutdown`) — implemented by the
  plugin, invoked through `&self` from many concurrently running tasks, so
  the plugin shares interior state (pools, caches) behind `Arc`.
- `serve_concurrent(client, jwt, handler)` — registers, runs `on_init`, then
  drives the loop to shutdown; plugin `main` functions no longer need their
  own `PLUGIN_ID`/`PLUGIN_VERSION` constants.
- `run_concurrent_loop(client, handler)` — the loop itself, for tests
  against a pre-registered client (`UnixStream::pair`).
- `response_envelope(action_id, result)` — the one shared
  `ActionResponse`/`ACTION_ERROR` builder both plugins had duplicated.
- The loop: one task owns the `VynkorClient` exclusively and
  `tokio::select!`s between `client.recv()` and an mpsc channel of completed
  responses; each inbound `ActionRequest` is `tokio::spawn`ed, so requests
  run concurrently and replies may come back out of order (kernel matches on
  `action_id`). The client is never behind a lock, so a replying handler
  can't deadlock against the loop parked in `recv()`. A panicking handler is
  caught by a double-spawn and becomes an `ACTION_ERROR` response instead of
  a silently dropped reply.
- Optional `accept(&req)` pre-spawn gate: run in the loop task to reject an
  over-cap request without spawning a task at all (`network`'s per-caller
  in-flight cap uses it; the authoritative reservation re-checks inside
  `on_action`, since a slot may have been taken between gate and
  acquisition).

`database` and `network` are migrated onto the SDK loop (their hand-rolled
`run_loop`/`spawn_handler`/`response_envelope` copies are deleted); all their
existing tests — including the deadlock-regression and per-caller-cap tests —
pass unchanged. `vector-db` and anything else on the hot path get the pattern for free by
implementing `ConcurrentHandler`. `notes`/`calendar`/`scheduler` don't
implement it — they're CRUD wrappers that call `database`
through a channel-fronted RPC proxy so their serve loop remains the single
reader of the connection (`send_action`'s discard-while-waiting would
otherwise eat inbound frames during handler or timer-driven outbound calls).
Rust only for these — no Python/C++
SDK versions of `database` or `vector-db`; hot-path plugins stay in the SDK
with the async pool story.

## Kernel-side changes needed (vynkor repo, not this one)

Most of the above needs **no** kernel change — `PERMISSION_NETWORK`,
`PERMISSION_FILES_READ`/`WRITE`, `PERMISSION_SYSTEM`, `PERMISSION_AUDIO`,
`PERMISSION_NOTIFY`, `PERMISSION_SCHEDULER`, `PERMISSION_BROWSER`,
`PERMISSION_IPC_SEND` already exist in
`wire/proto/vynkor_protocol.proto:107-124` and cover `filesystem`, `system`/
`window`, `notify`, `scheduler`, `browser` respectively. `stt`/`tts` shipped
with **zero** kernel changes (no declared permissions — local ONNX runs
in-process, cloud providers route through `network`).

What's actually new, in `vynkor`:

- **Proto enum addition — protocol v1.4.** **Shipped** (wire housekeeping,
  `vynkor-wire` 0.2.1): 5 new `PermissionType` values **15–19** defined
  (`PERMISSION_STORAGE = 14` shipped with `database`; 7 and old
  `PERMISSION_AI` are `reserved`, don't reuse):

  | Value | Permission | Plugin |
  |---|---|---|
  | 15 | `PERMISSION_SECRETS` | `secrets` |
  | 16 | `PERMISSION_CLIPBOARD` | `clipboard` |
  | 17 | `PERMISSION_LAUNCH` | `launcher` |
  | 18 | `PERMISSION_SCREEN` | `screenshot` |
  | 19 | `PERMISSION_HOME` | `home` |

  Values are **contiguous (15–19)** — the installer's
  `known_permissions()` probe (`vynkor/src/marketplace/installer.rs:25`)
  walks enum codes and stops after 4 consecutive misses, so a gap ≥4 would
  silently reject installs of any plugin declaring a later value. The
  `// v 1.4` header bump landed in the same change. The kernel's own `M9`
  (zero-value enum renumber, wire-breaking) **missed** this bump and landed
  on protocol **v1.5** (`vynkor-wire` 0.2.2, 2026-08-13): `ActionStatus`/
  `CommandStatus` now have `*_UNKNOWN = 0` so a missed `set_status()` fails
  loudly instead of faking OK.

  **Next batch (not yet defined in proto)** — when the new plugins land,
  values continue contiguously at **20–24**, same installer-probe
  constraint: `PERMISSION_WIFI` = 20 (`wifi`),
  `PERMISSION_BLUETOOTH` = 21 (`bluetooth`), `PERMISSION_INPUT` = 22
  (`input`), `PERMISSION_CAMERA` = 23 (`capture`), `PERMISSION_HOTKEY` =
  24 (`hotkey`). One wire bump covers all five; land them together or
  keep the gap rule in mind if split.
  `metrics`, and the extended `system` need no new values — they reuse
  existing ones (`sound` already shipped that way on `PERMISSION_AUDIO`).
- **Regenerate `vynkor-wire` prost types.** **Shipped** — the generated
  `PermissionType` (prost, build-time from the proto) includes the new
  values; `known_permissions()` (kernel `R8-01`) and the JWT `permissions`
  claims (free-form strings) adopt them automatically, no kernel Rust
  source change needed. `vyn plugin install` now accepts manifests
  declaring the new permissions (e.g. `PERMISSION_SECRETS`).
- **Proto-copy sync — all three copies on v1.4.** **Shipped** — the kernel
  repo vendors no proto (`src/proto.rs` is
  `pub use vynkor_wire::proto::vynkor;`), so the crate is the single source
  of protocol truth for kernel + SDK-rust. The R8-05 byte-identity test
  (`tests/unit/test_proto_sync.rs`) guards the remaining copies:
  - `vynkor-wire/proto/vynkor_protocol.proto` — the source of regeneration;
  - `vynkor-sdk-python/proto/...` + `vynkor-sdk-cpp/proto/...` — synced to
    v1.4 (previously on v1.2/v1.3); the Python binding
    (`vynkor-sdk-python/vynkor/vynkor_protocol_pb2.py`) was regenerated via
    `scripts/gen_proto_python.py` and the R8-05 marker check extended to
    the five new permission values.
  `pub const PROTOCOL_VERSION` was added to `vynkor-wire` alongside — it
  mirrors the proto header comment (now `"1.5"` / `// v 1.5`). Long-term:
  vendor the .proto as an asset inside the vynkor-wire crate and have SDK
  build scripts generate from the *installed package* — removes vendoring
  entirely, so the SDKs can't drift even in principle.
  (`scripts/gen_proto_python.py` was repaired earlier — it regenerates the
  Python binding from `../vynkor-wire/proto/` and works.)
- **`src/auth/permissions.rs::required_permission_for_action`** — only
  needs an entry if a new plugin's action is *providable through another
  plugin* (the anti-laundering pattern that exists for `http_request` →
  `PermissionNetwork` today). None of the planned plugins expose a
  primitive like that, so no additions expected — evaluate per-plugin as
  each one lands, not a bulk change now.
- **Read-only discovery commands for the agent — shipped (2026-08).**
  `list_plugins` (new `CommandHandler` arm: registered plugins with their
  actions) joined the existing `get_manifest`, and both are exempt from the
  `PERMISSION_KERNEL_ADMIN` gate alongside `health_check`
  (`READONLY_COMMANDS` in `vynkor/src/ipc/protocol.rs`). This is what lets
  the `agent` plugin pull tool specs from registered manifests without
  holding admin. Read-only by construction and the data is public
  distribution metadata (it ships in registry.json); every mutating command
  stays admin-gated.
- **`daemon`'s always-on lifecycle** — resolved by the kernel's R10-01/R10-04
  (2026-08): a plugin in `plugins.d/` is auto-spawned at boot and `vyn plugin
  enable|disable <slug>` toggles that. `daemon` needs no kernel change — just
  a drop-in file; no open question remains.

No new Envelope payloads, IPC, framing, or orchestrator changes needed:
every planned plugin fits the existing `ActionRequest`/`Event`/
`EventPublish`/IPC/streaming/`AudioStreamChunk` + WebSocket surfaces.

## Infrastructure Evolution: Plugin Distribution & Registry

A single distribution format for plugins, built so the format itself never
needs a breaking change (additive fields, lenient parsing) and the artifact
host is swappable (relative URLs). **Normative schema:** `vynkor/docs/
PLUGIN_REGISTRY_SCHEMA.md` (kernel repo) — this file is the plan, that doc is
the contract. `scripts/package.sh` is the one tool that writes both sides and
must stay in sync with the schema.

### Roles (delivery vs execution separation)

- **`plugin.json`** (manifest) — *execution*: what runs, what it can do.
  Lives inside the archive; the in-archive copy is authoritative.
- **`registry.json`** — *delivery index*: what's available, where, per-version
  sha256/signature. The machine source of truth the kernel reads.
- **`dist/`** — *artifact store*: co-located per-version files for humans,
  ops, and at-rest audit. **Not consumed by the kernel.** `package.sh`
  generates the registry entry and the dist files from one computation, so
  the two representations cannot drift.

### 1. Distribution Store (`dist/`) — hierarchical

```
dist/{slug}/
├── latest.json                    # {"version": "0.2.0"} — host-agnostic pointer
├── assets/                        # version-agnostic: icon.png, setup.md, dependencies.json
└── versions/{version}/
    ├── {slug}-{version}.zip       # binary archive
    ├── {slug}-{version}-src.zip   # source archive (audit)
    ├── plugin.json                # manifest of this version (browse without downloading)
    ├── checksum.sha256
    └── signature.sig
```

- **Version isolation**: one folder per release → retention, rollback,
  partial mirroring, per-folder CDN cache control, per-plugin storage
  management on a self-hosted VPS.
- **`latest.json` instead of a symlink**: GitHub raw / static CDNs do not
  follow symlinks. The kernel does not depend on it either — it resolves
  latest as semver-max over the registry `versions` map **among entries with
  `status: stable` (or absent status), falling back to any version when no
  stable exists** (zero drift). `latest.json` is for humans/ops/mirroring.
- **`assets/`** (not `resources/`) to avoid confusion with the manifest's
  `files` field. `dependencies.json` here lists *system* packages for the
  kernel's optional auto-check — distinct from the registry's plugin
  `dependencies`.
- The per-version `plugin.json`/`checksum.sha256`/`signature.sig` are for
  manual verification and browsing; the registry carries the same values
  (same computation, two outputs).
- **The browse-copy `plugin.json` is NOT covered by the entry signature**
  (only the zip's sha256 is signed). The kernel must never read it — the
  authoritative manifest is the one inside the zip, which IS covered via the
  signed zip hash. Browse copy is humans-only.

### 2. Registry Evolution (`registry.json`)

Array → object map keyed by slug. The kernel parser already accepts this form
and the R10-03 cache is ready:

```json
{
  "meta": { "apiVersion": 2, "lastUpdated": "2026-08-13" },
  "revoked": ["evil@1.0.0"],
  "ai": {
    "name": "AI",
    "description": "Provider-agnostic LLM chat completion.",
    "category": "ai",
    "tags": ["llm"],
    "status": "stable",
    "source_url": "https://github.com/vynkor-core/vynkor-plugins/tree/main/plugins/ai",
    "versions": {
      "0.1.0": {
        "archive_url": "dist/ai/versions/0.1.0/ai-0.1.0.zip",
        "sha256": "<hex>",
        "signature": "<hex>",
        "min_kernel_version": "0.1.0",
        "max_kernel_version": "*",
        "dependencies": { "network": ">=0.1.0" }
      }
    }
  }
}
```

- **`archive_url`** — the registry ships **absolute** URLs
  (`https://raw.githubusercontent.com/...` today); the kernel's R10-03 parser
  also accepts **relative** ones resolved against the registry's own base URL.
  Moving the store GitHub → own VPS → Cloudflare R2, or pointing at a
  community marketplace, is a one-line `registry_url` change in config.yaml.
  Nothing gets re-published.
- **No permissions in the registry** — execution metadata lives in the
  manifest (inside the archive); duplicating it in the registry would only
  drift. `vyn plugin search` surfaces name/description/category instead.
- **`status`** (`stable`/`beta`/`deprecated`/`hidden`/`revoked`) — only
  `revoked` is kernel-enforced (R10-03): the root `revoked: ["slug",
  "slug@version"]` list folds into entries, `vyn install` refuses, entries
  stay listed with a `[revoked]` marker, and revocation outlives the cache
  TTL. Default at slug level; an optional `versions[].status` overrides it
  per version (e.g. `0.1.0` stable, `0.2.0` beta).
- **`dependencies: { "slug": ">=semver" }`** — install-time, transitive:
  `vyn install` resolves and installs prerequisites first, refuses on version
  mismatch. **Kernel-enforced** — a plugin whose deps aren't installed is not
  installable. Load-time ordering stays the manifest's existing `requires`
  (already enforced: missing deps / cycles refuse the plugin).
  **Range syntax is deliberately limited to `>=x.y.z` or exact `x.y.z`** —
  no caret/tilde/AND-OR. The resolver stays a simple recursive walk with
  cycle detection (same shape as `requires`); a full npm-style resolver is
  out of scope for the dumb kernel.
- **`meta`** — lastUpdated + apiVersion for cache invalidation (R10-03
  echoes it into `registry-cache.json`, accepts `apiVersion`/`api_version`).
- **One active registry per install** — `registry_url` + `marketplace_public_key`
  config.yaml overrides already exist. A community marketplace is another URL
  + key the operator chooses to pin (entries must verify against that key).
  Multi-registry aggregation/search is future work and not blocked by the
  format.

### 3. Manifest Optimization (`plugin.json`)

- **Clean-up**: remove delivery data (`archive_url`, `sha256`) from the
  manifest — it lives in the registry now.
- **Per-action specification**: `actions` become objects, not strings:
  `[{ "name": "http_request", "permission": "network", "input": {...}, "output": {...} }]`.
  The per-action `permission` makes the kernel's anti-laundering check
  (`required_permission_for_action`, today hardcoded for `http_request` →
  `PERMISSION_NETWORK`) **data-driven**: any caller without the permission is
  denied, whatever the action. Input/output schemas serve Vynkor Web and the
  future `agent` tool dispatch. The declared `permissions` set stays as-is —
  kernel Steps 3/4 (unknown permission, config-grant cross-check) unchanged.
- **`config_schema`** — JSON Schema (draft-07 subset), not a custom format.
  Vynkor Web auto-generates settings forms; the plugin validates its own
  config. The kernel does not validate (dumb core).
- **`files`** (renamed from `resources`) — explicit list of files extracted
  from the archive into the plugin's working directory. Doubles as the
  **extraction allowlist**: the installer extracts only the declared files
  and ignores the rest (tighter than "extract everything" on top of the
  zip-bomb limits). Renamed to avoid confusion with `dist/{slug}/assets/`.
- **No `api_level`** — decided against. The kernel is a dumb router; its
  plugin-visible contract is the wire format + the permission enum, both of
  which live in `vynkor-wire` (below). Compatibility is fully covered by:
  `kernel_compatibility_range` (semver — the gate), the installer's
  `known_permissions()` probe (new permissions are adopted automatically when
  the kernel bumps its vynkor-wire dependency), and additive/lenient manifest
  parsing (unknown fields ignored). A separate api_level axis would need a
  mapping table maintained forever — YAGNI. If a plugin-visible kernel
  behavior ever genuinely needs gating, add one optional manifest field then.

### 4. Protocol single source (`vynkor-wire`)

The kernel already consumes every protocol type from the crate: `src/proto.rs`
is `pub use vynkor_wire::proto::vynkor;` and `known_permissions()` probes the
generated `PermissionType`. A protocol/permission change is therefore already
"bump vynkor-wire → kernel + SDK-rust adopt via the dependency." Remaining work:

- ~~Add `pub const PROTOCOL_VERSION` to vynkor-wire~~ — **done** (0.2.1,
  `"1.4"`); it now mirrors the proto header comment.
- ~~Sync the vendored copies + fix `gen_proto_python.py`~~ — **done**:
  `vynkor-sdk-python/proto` and `vynkor-sdk-cpp/proto` are on v1.4 (they
  were on v1.2/v1.3), the Python binding was regenerated, and the R8-05
  byte-identity test + pb2 marker check guard them (markers extended to the
  new permission values). `gen_proto_python.py` was repaired earlier and
  verified working.
- Long-term: vendor the .proto as an asset inside the vynkor-wire crate and
  have SDK build scripts generate from the *installed package* — removes
  vendoring entirely, so the SDKs cannot drift even in principle.

### 5. Signing

Trust model (T-11): Ed25519 over the **S1 canonical message**
`{slug}:{version}:{sha256}:{status}:{archive_url}:{min_kernel_version}:{max_kernel_version}`
— the whole delivery surface is bound, so a compromised serving channel can't
flip `revoked → stable`, redirect `archive_url`, or loosen the compat bounds
without breaking the signature. Verified against the pinned maintainer public
key (`official_source()` in vynkor-manager). The canonical form lives in
`vynkor-manager/src/registry.rs::signed_message`; `scripts/package.sh`
signs it at packaging time (key from env/file, never committed) and
`scripts/resign.py` re-signs published entries without rebuilding archives
(`--check` verifies existing signatures against the pin). Host migration
never touches keys — only `registry_url`.

### Sequencing

1. **Registry v2 + dist/ hierarchy + package.sh** — map form, relative URLs,
   `dependencies`, new dist layout, signing step. Kernel is already tolerant;
   one PR in this repo. **Shipped** (PR #5).
2. **wire housekeeping** — `PROTOCOL_VERSION` const, sync SDK copies to v1.4,
   fix `gen_proto_python.py`. **Shipped** — wire is at protocol **v1.5**
   (5 new `PermissionType` values 15–19 landed in v1.4, status-enum renumber
   M9 in v1.5) with `vynkor_wire::PROTOCOL_VERSION`;
   `vynkor-sdk-python` + `vynkor-sdk-cpp` vendored copies and the Python `pb2`
   binding synced (R8-05 byte-identity + marker checks pass); Rust
   `vynkor-sdk` restored to the published 0.1.2 API surface (streaming methods
   had gone missing from the repo) and bumped to 0.1.3. **Both crates are
   published** (2026-08-13) — `vynkor-wire` **0.2.2**, `vynkor-sdk` **0.1.3** —
   and the kernel's `[patch.crates-io]` git overrides are **dropped**
   (`gen_proto_python.py` had already been repaired in an earlier PR —
   verified working, no change needed).
3. **Manifest v2** — per-action permissions + `config_schema`; touches every
   plugin, kernel load-time checks, and Vynkor Web. **Shipped for plugins +
   kernel** (all 6 manifests are v2, kernel parses object-form `actions`,
   enforces `files` extraction allowlist, and the anti-laundering check is
   data-driven from per-action `permission`). Vynkor Web consuming
   `input`/`output`/`config_schema` for form generation is still open.


- No plugin-to-plugin direct calls — everything routes through the kernel,
  same as `ai` → `network` today.
- No new kernel-level scheduling/timer primitive — `scheduler` is an
  ordinary plugin publishing to the event bus / firing actions on a timer,
  matching the "zero-AI/zero-scheduling in kernel core" precedent already
  set for `ai` (`plugins/ai/ROADMAP.md`, "Non-goals" section).
- `vector-db` stays a separate plugin from `database`, not a mode of it —
  different backend, different access pattern (similarity search vs
  relational/KV), same reasoning that kept `ai` from reinventing `network`'s
  HTTP client.
