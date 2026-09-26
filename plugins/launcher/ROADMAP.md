# launcher plugin roadmap

Local app/game launch for vynkor — six providers, one permission.

## v0.1 — shipped (0.1.0)

- `launch` / `launch_list` / `launch_providers` over `PERMISSION_LAUNCH`.
- **desktop** — scan `LAUNCHER_DESKTOP_DIRS` for `.desktop` (Name/Exec/Terminal/Path/NoDisplay/Hidden, field-code strip, `exec_argv` quote handling), launch via `gtk-launch` → `gio` → `xdg-open` → direct `Exec` argv. `Terminal=true` entries wrapped with `LAUNCHER_TERMINAL` (tmux/kitty/alacritty/ghostty) + `Path` as `-c`/`--working-directory`. `include_hidden` gate, deduplicate by stem.
- **steam** — scan `LAUNCHER_STEAM_ROOTS/*/steamapps` for `appmanifest_*.acf` + `libraryfolders.vdf` additional libs, parse `appid`/`name`, launch via `steam steam://rungameid/<appid>` (`LAUNCHER_STEAM_BINARY`).
- **tmux** — `tmux ls` sessions as catalog, `launch` does `new-window -d -t <session>` or `new-session -d -s <session>` with `-c <Path> -- <cmd>`. `LAUNCHER_TMUX_SESSION` default `vynkor`.
- **kitty / alacritty / ghostty** — direct terminal wrappers: `kitty -- <cmd>`, `alacritty -e <cmd>`, `ghostty -e <cmd>`, with `--directory`/`--working-directory` from `Path`. Accept any `app_id` as command when not found in catalog.
- **cache** — in-memory scan cache `LAUNCHER_CACHE_TTL_MS` default 60000 (0 disables), per-provider (desktop/steam), `include_hidden` filtered from full cache.
- Security: argv-only spawn (never shell), allowlist `LAUNCHER_ALLOWED_IDS`, caps (`app_id` 256, `args` 32×1024, null-byte reject), no path traversal (ids are stems, not paths).
- Config: `DESKTOP_DIRS` / `STEAM_ROOTS` / `ALLOWED_IDS` / `TIMEOUT_MS` / `STEAM_BINARY` / `DESKTOP_LAUNCHER` / `TERMINAL` / `TMUX_SESSION` / `CACHE_TTL_MS`.
- Tests: 35 unit + 5 fake-kernel e2e (`UnixStream::pair`, handshake, dry_run, list, not_found, bad_params, terminal wrapping).

## Next

- `launch_list` pagination + `categories` filter from `.desktop` `Categories=`.
- Steam `libraryfolders.vdf` v2 `contentstats` handling, `compatdata` check.
- Flatpak provider (`flatpak run <appid>`) as third provider behind feature flag.
- `LAUNCHER_TIMEOUT_MS` enforcement on detached spawn (currently fire-and-forget, reserved).
- Move to `vynkor-sdk` concurrent loop if launch volume ever needs it (currently low-volume, sequential `Plugin::serve` is fine).

## Non-goals

- No URL/file open (`xdg-open https://` belongs to `browser`/`filesystem`).
- No process kill/management.
- No window focus — that belongs to `window` plugin.
- No remote launch.

## Audit 2026-09-27

- **Recents** — remember launches (`database`) and rank them first in
  `launch_list`; "open the last game" (EXI-10 remainder — fuzzy matching
  already shipped).
