# filesystem plugin

Sandboxed local file browse/read/write for vynkor plugins. Seven actions:
`fs_list` (read-only directory browse), `fs_read` (windowed file read),
`fs_write` (full-file create/overwrite), `fs_delete` (permanent or
freedesktop-trash removal), `fs_mkdir` (directory creation), `fs_rename`
(rename within a root), and `fs_move` (move across directories). No exec, no
shell.

The plugin touches **only** absolute paths inside an operator-configured
allowlist of directory roots (`FILES_PLUGIN_ALLOWED_ROOTS`). Unset or empty
means deny-all: every action is rejected until the operator configures at
least one root.

## Operator note

`filesystem` declares two kernel permissions — `files_read` and `files_write`
(`plugin.json`: `"permissions": ["files_read", "files_write"]`) — mapped
per-action (`fs_list`/`fs_read` → `files_read`; `fs_write`/`fs_delete`/
`fs_mkdir`/`fs_rename`/`fs_move` → `files_write`).
It opens no sockets and spawns no processes; all I/O is plain `std::fs`, so it
is safe to run with `sandbox: true`.

## Sandbox model

Every requested path goes through one resolution algorithm
(`src/sandbox.rs`):

1. The path must be **absolute**.
2. The deepest *existing* ancestor is canonicalized — this resolves every
   symlink component in the existing portion of the path.
3. A `..` that would survive into the non-existing remainder is rejected
   outright (`ERR_FILES_PATH_TRAVERSAL`). `..` inside the existing portion is
   harmless: canonicalize already folded it.
4. The non-existing remainder is re-joined textually and the result must be
   component-wise inside one of the canonical roots
   (`ERR_FILES_PATH_ESCAPES_ROOT` otherwise).

This blocks:

- `..` traversal out of a root (`<root>/a/../../etc/passwd`);
- file symlinks pointing outside a root (canonicalize resolves them before
  the containment check);
- symlinked directory components (`<root>/linkdir/file.txt` where `linkdir`
  targets outside);
- writes through dangling symlinks (the final component is checked with
  `symlink_metadata`; `ERR_FILES_SYMLINK`).

Known limitation: check-then-use TOCTOU (a concurrent symlink swap between
resolution and I/O) is out of scope for v1 — see `ROADMAP.md`.

## Actions

### `fs_list`

```json
{ "path": "/srv/data", "include_hidden": false }
```

- `path` — required, absolute directory inside an allowed root.
- `include_hidden` — optional, default `false` (dotfiles skipped).

Response:

```json
{
  "path": "/srv/data",
  "entries": [
    { "name": "sub", "kind": "dir", "size_bytes": 4096, "modified_unix_ms": 1755000000000 },
    { "name": "notes.txt", "kind": "file", "size_bytes": 12, "modified_unix_ms": 1755000001000 }
  ],
  "truncated": false
}
```

- `kind` — `file` | `dir` | `symlink` | `other`, classified via `lstat`
  (symlinks report their own kind, not their target's).
- Entries are sorted dirs-first, then by name; capped at
  `FILES_PLUGIN_MAX_LIST_ENTRIES` (default 1000) with `truncated: true`.
- `modified_unix_ms` is `null` when the filesystem doesn't provide mtime.

### `fs_read`

```json
{ "path": "/srv/data/notes.txt", "offset": 0, "max_bytes": 65536 }
```

- `offset` — optional byte offset, default `0`.
- `max_bytes` — optional read window; defaults to
  `FILES_PLUGIN_MAX_READ_BYTES` (1 MiB) and is hard-capped at 8 MiB either way.

Response:

```json
{ "data": "hello", "encoding": "utf8", "size_bytes": 5, "truncated": false }
```

- `encoding` is `"utf8"` only when the whole file was read from offset 0 and
  the bytes are valid UTF-8; every other case (partial window, `offset > 0`,
  binary content) returns `"base64"`.
- `size_bytes` counts returned bytes pre-encoding; `truncated` is true when
  the window ended before EOF.

### `fs_write`

```json
{ "path": "/srv/data/out/report.bin", "content_base64": "AAEC", "create_parents": true }
```

- Exactly one of `text` / `content_base64` — required.
- `create_parents` — optional, default `false`; when the parent directory is
  missing the call fails unless this is set.
- Semantics: full-file create-or-overwrite (no append in v1). Refused when
  the target exists as a directory, is an allowed root itself, or is a
  symlink (`ERR_FILES_SYMLINK`).

Response: `{ "written_bytes": 3, "path": "/srv/data/out/report.bin" }`
(`path` echoes the resolved canonical path).

### `fs_delete`

```json
{ "path": "/srv/data/old.txt", "to_trash": true }
```

- `path` — required, absolute path inside an allowed root.
- `to_trash` — optional, default `true`: move to the freedesktop Trash
  (reversible) instead of permanent deletion.

Response: `{ "deleted": true, "trashed": true, "path": "/srv/data/old.txt" }`.
Refused when `path` is an allowed root itself.

### `fs_mkdir`

```json
{ "path": "/srv/data/new", "parents": false }
```

- `parents` — optional, default `false`; create intermediate directories.

Response: `{ "created": true, "path": "/srv/data/new" }` (`created` is
`false` when the directory already exists). Refused (`ERR_FILES_EXISTS`) when
the path exists as a non-directory.

### `fs_rename`

```json
{ "from": "/srv/data/a.txt", "to": "/srv/data/b.txt", "overwrite": false }
```

- `from`, `to` — required, absolute paths inside allowed roots.
- `overwrite` — optional, default `false`; refuses when the destination
  already exists unless set.

Response: `{ "renamed": true, "from": "...", "to": "..." }`.

### `fs_move`

```json
{ "from": "/srv/data/a.txt", "to": "/srv/data/sub/b.txt", "overwrite": false }
```

- Same semantics as `fs_rename`; cross-directory moves work.

Response: `{ "moved": true, "from": "...", "to": "..." }`.

## Configuration

Environment variables set in the kernel's `config.yaml` under this plugin's
`env:` list — see `config.example.yaml`.

| Variable | Default | Meaning |
|---|---|---|
| `FILES_PLUGIN_ALLOWED_ROOTS` | *(unset = deny-all)* | Comma-separated absolute directories the plugin may touch. Relative/nonexistent entries are logged and skipped. |
| `FILES_PLUGIN_MAX_LIST_ENTRIES` | `1000` | Cap on `fs_list` entries per call. |
| `FILES_PLUGIN_MAX_READ_BYTES` | `1048576` | Default `fs_read` window; hard cap 8388608 (8 MiB). |

```yaml
plugins:
  - id: filesystem
    binary: /opt/plugins/filesystem
    sandbox: true
    env:
      - FILES_PLUGIN_ALLOWED_ROOTS=/srv/data,/home/user/share
```

## Testing

`cargo test` — no network, no kernel: request parsing, sandbox resolution
(traversal, symlink escapes, deny-all), and all seven actions against real
`tempdir` fixtures (sorting, hidden files, caps, encoding rules, truncation
flags, write/delete/mkdir/rename/move behaviors).
