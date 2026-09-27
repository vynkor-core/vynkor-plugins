# database plugin roadmap

v1 scope, isolation model, concurrency design, and kernel dependency are
fully specified in
`docs/superpowers/specs/2026-07-15-database-plugin-design.md` — this file
only tracks what's deliberately deferred.

## Implemented since the original spec

- **Per-caller disk quota.** Was a non-goal; now enforced by SQLite itself via
  `PRAGMA max_page_count`, configured with `max_db_bytes`
  (`DATABASE_PLUGIN_MAX_DB_BYTES`, default 256 MiB). A write that would grow a
  caller's file past the ceiling fails with `SQLITE_FULL` rather than growing
  unbounded. This is the real cap on raw `db_query` writes — `max_value_bytes`
  only guards the `db_set` fast path and is trivially bypassed by a raw
  `INSERT` (e.g. `value || value`, `zeroblob(...)`). `0` disables it.

- **Internal streaming for `db_query`.** The result cap is now enforced
  incrementally while streaming rows off the connection (`Executor::fetch_many`)
  instead of after materializing the whole set, so an oversized `SELECT` is
  rejected before it can balloon plugin memory. The same rewrite fixed
  `INSERT … RETURNING` (a row-producing write) silently dropping its returned
  rows. Note this is *internal* — callers still receive one whole
  `{rows, rows_affected}` response bounded by `max_response_bytes`, not a
  chunked/streamed result set (see non-goals).

## Non-goals (v1, from the design spec)

- No TTL/expiry on KV entries.
- No cross-caller/admin actions (no list-namespaces, no admin query).
- No caller-facing streaming/chunked query responses — one bounded response
  per `db_query`, capped by `max_response_bytes`.
- Not a vector store (`vector-db` is its own plugin per the root `ROADMAP.md`).

## Near-term follow-ups

- Swap the local path-override `vynkor-wire`/`vynkor-sdk` dependencies (see
  `Cargo.toml`) for published crates.io versions once the kernel changes in
  `docs/superpowers/plans/2026-07-17-database-plugin-kernel-support.md` are
  released.
- `notes`/`calendar` (root `ROADMAP.md` "Planned" table) become buildable
  once this ships — they're thin schema layers on top of `db_query`.
- `scheduler` depends on this plugin for persisting schedule state across
  kernel restarts (root `ROADMAP.md`).

## v0.3 — feature batch (2026-08, agreed)

All buildable now — no kernel changes. Status updated after implementation:

- **`db_incr {key, delta?}`** — atomic integer counter (default delta `1`,
  negatives allowed; starts at `delta` when the key is missing; errors when
  the stored value is not an integer). Returns `{ok, value}`. Status: **done**.
- **`db_keys {prefix?}`** — list key names, optional prefix filter
  (`%`/`_` escaped), sorted. Returns `{keys}`. Status: **done**.
- **`db_append {key, value}`** — atomic append to a JSON-array value; starts
  a new array when the key is missing; errors when the stored value is not
  an array. Returns `{ok, length}`. Status: **done**.
- **KV TTL** — `db_set {ttl_ms?}` stores an `expires_at` column (schema
  migration for existing `.db` files); every action sweeps expired rows
  first, and accessors treat expired keys as missing. Deliberately reverses
  the v1 "no TTL" non-goal (agent caches need it). Status: **done**.
- **`db_patch {key, path, value}`** — JSON-path update via SQLite
  `json_set` (`$.a.b`, `$[0]`); errors when the key is missing. Returns
  `{ok, value}`. Status: **done**.
- **Change events** — mutations publish a best-effort
  `plugin.database.changed` event `{caller, action, key}` (same
  fire-and-forget pattern as `network.request_completed`); the manifest
  gains `PERMISSION_EVENT_PUBLISH`. Status: **done**.

Deferred (above medium cost): multi-statement transactions (BEGIN/COMMIT
across calls needs a transaction-pinning design against the pooled
connections).

## STAT-01 (audit 2026-09-27) — shipped in 0.1.1

The bare `status` action is now `database_status`. The kernel refuses to route
an action declared by more than one registered plugin (`ACTION_NOT_FOUND`),
and nine plugins declared `status`, so none of them was reachable.
