# filesystem plugin roadmap

Sandboxed local file browse/read/write: `fs_list` / `fs_read` / `fs_write` /
`fs_delete` / `fs_mkdir` / `fs_rename` / `fs_move` over an operator-configured
allowlist of absolute directory roots. No outbound RPC, no network, no secrets
— plain `std::fs` behind the sandbox in `src/sandbox.rs`.

## v1 scope (shipped, 0.1.0)

- `fs_list` — read-only browse: entries with lstat kinds (`file`/`dir`/
  `symlink`/`other`), dirs-first sort, dotfile filter, entry cap +
  `truncated` flag.
- `fs_read` — windowed read (`offset`/`max_bytes`, hard cap 8 MiB),
  utf8-or-base64 encoding rule (utf8 only for a whole-file read from
  offset 0 that is valid UTF-8), `truncated` flag.
- `fs_write` — full-file create-or-overwrite from `text` xor
  `content_base64`, opt-in `create_parents`.
- `fs_delete` — permanent removal or freedesktop-trash move (`to_trash`
  default true).
- `fs_mkdir` — directory creation with opt-in `parents`.
- `fs_rename` / `fs_move` — rename/move within allowed roots, opt-in
  `overwrite` (cross-directory moves supported).
- Sandbox: absolute paths only; deepest-existing-ancestor canonicalize;
  `..` surviving into the non-existing remainder rejected; containment
  check against canonical roots; symlink-final-component refusal on write.
- Default-deny: unset/empty `FILES_PLUGIN_ALLOWED_ROOTS` rejects every
  action.

## Non-goals (v1)

- **No exec, no shell** — ever. This plugin reads and writes bytes; running
  programs is what the kernel's supervisor model exists to prevent. This is
  the reason the previously considered `shell` plugin was rejected (root
  `ROADMAP.md`, "Considered and skipped").
- **No append mode** — v1 writes are whole-file only; partial mutation is
  better served by `database`'s KV primitives.
- **No streaming/chunked write** — one request carries the whole payload;
  large-file upload wants a streaming envelope first.
- **TOCTOU** — the sandbox resolves a path, then I/O happens on it; a
  concurrent symlink swap between the two is not defended against in v1.
  Defending it needs openat2-style `RESOLVE_BENEATH` semantics (Linux-only)
  or re-verification after open; revisit if multi-writer roots become a
  real deployment shape.
- **No watch/notification** — file-change events belong to a future design,
  not bolted onto this plugin (sketched in "Future" below — they land here,
  same roots/permission model, once the subscription lifecycle is designed).

## Near-term ideas

- `fs_stat` — single-entry metadata probe without listing the parent dir.
- Root-scoped relative paths (`{"root": "data", "path": "a/b.txt"}`) so
  callers don't need to know host absolute paths — convenience only, the
  sandbox already enforces containment.
- Hash digest option on `fs_read` (`sha256`) for integrity checks by
  callers like a future sync engine.

## Unblocks

- `launcher` (root `ROADMAP.md` Planned table): reading app manifests /
  `.desktop` files via `fs_read` once its own permission model lands.

## Future (unscheduled)

- **`fs_watch_start` / `fs_watch_stop` + `fs.changed` events** — inotify
  over the allowed roots, published through the kernel event bus
  (`PERMISSION_EVENT_PUBLISH`; event-loop precedent: calendar's reminder
  scan, media's D-Bus watcher). The automation trigger source: "new file
  in Downloads → OCR → notes". Deliberately not in v1 (see non-goals) —
  wants a design pass on subscription lifecycle first: who owns the
  watcher, what happens on plugin restart, event coalescing for bursty
  writes.
- **Trash actions** — freedesktop-trash-aware `fs_trash_list` /
  `fs_trash_restore` / `fs_trash_purge`, scoped to the same allowed roots:
  listing/restoring/purging what `fs_delete {to_trash}` (which already moves
  to Trash) has put there.
