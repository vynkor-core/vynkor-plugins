# clipboard plugin

Read/write the system clipboard for vynkor plugins. v1 is text-only and
local: it spawns host clipboard binaries directly with argv — never a shell —
so clipboard content cannot inject commands (same delivery model as
`notify`). Declares `PERMISSION_CLIPBOARD` (proto v1.4).

## Status

v0.1.0 — `clipboard_read` / `clipboard_write` / `clipboard_providers` over
`wl-paste`/`wl-copy` (Wayland) and `xclip`/`xsel` (X11), plus a persisted
history (`clipboard_history` / `clipboard_clear`) in `database`. Session
detected from the environment; provider override via config.

## Providers

| Session | Read | Write |
|---|---|---|
| Wayland (`WAYLAND_DISPLAY`) | `wl-paste --no-newline` | `wl-copy` (text via stdin) |
| X11 (`XDG_SESSION_TYPE=x11` or `DISPLAY`) | `xclip -selection clipboard -out`, fallback `xsel --clipboard --output` | `xclip -selection clipboard -in`, fallback `xsel --clipboard --input` |

Detection order: `WAYLAND_DISPLAY` → `XDG_SESSION_TYPE=x11` → `DISPLAY`.
Nothing set → `ERR_CLIPBOARD_NO_SESSION`. A missing binary falls through to
the next in the chain; all missing → `ERR_CLIPBOARD_PROVIDER_MISSING`
listing what was tried.

## Actions

| Action | Params | Result |
|---|---|---|
| `clipboard_read` | — | `{ found, text, provider }` — `found:false` + `text:null` when the clipboard is empty |
| `clipboard_write` | `text` (non-empty string) | `{ ok, provider, bytes }` |
| `clipboard_providers` | — | `{ session, readers, writers }` |
| `clipboard_history` | `query?` (case-insensitive substring), `limit?` (1..500, default 100), `offset?` | `{ entries, total }` — newest first |
| `clipboard_clear` | — | `{ cleared }` — erases the stored history only, never the live clipboard |

### History

Every successful `clipboard_read` (non-empty) and `clipboard_write` through
this plugin is appended to a ring in the plugin's own `database` namespace
(`CLIPBOARD_PLUGIN_HISTORY_LIMIT`, default 1000; `CLIPBOARD_PLUGIN_HISTORY=off`
disables capture). There is no background watcher: text copied in other
apps is recorded only when something calls `clipboard_read`.

`clipboard_history` is risk **critical** and `requires_confirmation` — the
log outlives the live clipboard and can surface old passwords or tokens.

## Error taxonomy

`ERR_CLIPBOARD_NO_SESSION` / `PROVIDER_MISSING` / `TIMEOUT` /
`TOO_LARGE` / `BAD_PARAMS` / `READ_FAILED` / `WRITE_FAILED`.

## Security model

- argv-only spawn of well-known binaries; no shell, so content is never
  interpreted.
- Size cap on both directions: reads above the cap are rejected
  (`TOO_LARGE`), writes are rejected before spawn.
- Per-spawn timeout; a hung backend is killed and reported as `TIMEOUT`.
- Empty writes rejected at parse time (an accidental clear looks identical
  to an intentional one — make callers say it explicitly once a clear action
  exists).
- Text/UTF-8 only; non-UTF-8 clipboard content is `READ_FAILED`.

## Config

Config is read from the environment (the kernel's plugin supervisor
translates the `config.yaml` `env:` entry into these before spawning the
plugin).

| Env var | Default | Meaning |
|---|---|---|
| `CLIPBOARD_PLUGIN_PROVIDER` | `auto` | Provider preference: `auto` / `wayland` / `x11` |
| `CLIPBOARD_PLUGIN_TIMEOUT_MS` | `5000` | Per-spawn timeout in milliseconds |
| `CLIPBOARD_PLUGIN_MAX_BYTES` | `1048576` | Hard cap on payload size in bytes |
