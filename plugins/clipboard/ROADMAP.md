# clipboard plugin roadmap

Text-only system clipboard access via host binaries — one blessed path for
`read`/`write` over the detected graphical session.

## v1 — shipped (0.1.0, local-only)

- `clipboard_read` / `clipboard_write` / `clipboard_providers`.
- Wayland: `wl-paste --no-newline` / `wl-copy`; X11: `xclip` → `xsel`
  fallback chains. Session detection: `WAYLAND_DISPLAY` →
  `XDG_SESSION_TYPE=x11` → `DISPLAY`; operator override via
  `CLIPBOARD_PLUGIN_PROVIDER`.
- argv-only spawn, never a shell (notify precedent). Size cap
  (`CLIPBOARD_PLUGIN_MAX_BYTES`, default 1 MiB) and per-spawn timeout
  (`CLIPBOARD_PLUGIN_TIMEOUT_MS`, default 5s).
- `Runner` trait boundary (`RealRunner` / `FakeRunner`) — 21 tests, no real
  compositor needed in CI.
- Declares `PERMISSION_CLIPBOARD` (proto v1.4, value 16).

## Known bugs (live-kernel audit 2026-08-22)

> **Fixed 2026-08 (`fix/live-audit-defects`, merged):** the spawn → stdin →
> wait path now uses a real `tokio::time::timeout` with `kill_on_drop(true)`,
> and writers (`wl-copy`/`xclip -in`) complete on direct-child exit instead
> of waiting for daemon-inherited pipe EOF — `clipboard_write` is ~25 ms.
> See `docs/LIVE_KERNEL_AUDIT_2026-08-22.md` defect #1.

- **`clipboard_write` takes 35–60+ s** on Wayland while `clipboard_read`
  is 3–22 ms and the same `wl-copy` binary answers instantly from a
  shell. Found in the first full live-kernel audit
  (`docs/LIVE_KERNEL_AUDIT_2026-08-22.md`, defect #1); latency varies
  between runs, so the wait is not a fixed timeout. Working hypothesis:
  `wl-copy` daemonizes to keep serving the selection, and our spawn/wait
  path waits on something the daemonized child keeps alive (fd or pipe),
  only unwinding when some outer timeout trips. Fix direction: spawn
  `wl-copy` with stdin from the payload and detach its stdio
  (`Stdio::null()`/`make_contiguous` equivalent) so no inherited pipe pins
  the wait; verify against `wl-copy --foreground` semantics; add a
  regression test with a fake runner that emulates a daemonizing child.

## Later (unscheduled)

- Clearing the *live* clipboard — still open (Wayland has no standard
  clear; xclip clears only the selection it owns). The shipped
  `clipboard_clear` erases the stored history only.
- Non-text MIME (images/HTML) behind a feature flag — needs a different
  transport than argv/stdin and a size policy of its own.
- ~~Clipboard history / multi-slot~~ — **shipped**: `clipboard_history` /
  `clipboard_clear` over `database` (EXI-06). Open follow-ups: dedupe
  consecutive identical entries, a secret filter (skip entries that look
  like tokens/passwords, or honor the `x-kde-passwordManagerHint` MIME
  hint password managers set), and an optional watcher
  (`wl-paste --watch`) so copies made in other apps are recorded without a
  `clipboard_read`.
- Primary-selection support (`--primary` / `-selection primary`) if a caller
  actually needs it.

## Non-goals

- No network sync between machines.
- No daemon/watch mode *by default* — reads are on-demand spawns. An
  opt-in watcher for history is listed under "Later".
- No new `PermissionType` enum value — `PERMISSION_CLIPBOARD` already exists.

## References

- wl-clipboard: https://github.com/bugaevc/wl-clipboard
- xclip: https://github.com/astrand/xclip
