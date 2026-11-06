# chats

Host-wide conversation history for every device: sessions, turns, full-text
search (SQLite FTS5) and revision-based sync. One SQLite file
(`$CHATS_DATA_DIR/chats.db`) per host; history is shared across callers.

Status: C1 (storage). Summaries, vectors and compaction arrive later.

## Configuration (environment)

| Variable | Default | Meaning |
|---|---|---|
| `CHATS_DATA_DIR` | required | directory holding `chats.db` |
| `CHATS_PAUSE_SECS` | 1800 | silence after which a voice session ends |
| `CHATS_TRACE_MAX_CHARS` | 200 | tool_trace `args`/`result`/`error` cut to this |
| `CHATS_TEXT_MAX_BYTES` | 65536 | `user_text`/`answer_text` cut to this (char boundary, `…` suffix) |
| `CHATS_SYNC_MAX` | 500 | upper bound for a `chat_sync` page |

## Actions

| Action | Params | Returns |
|---|---|---|
| `chat_session_resolve` | `device, kind, source?, session_id?` | `{session_id, created}` |
| `chat_turn_append` | `session_id, user_text, answer_text, status, tool_trace?, goal_id?` | `{turn_id, seq, rev}` |
| `chat_sessions_list` | `from?, to?, kind?, source?, limit?(50, max 200), before?(cursor)` | `{sessions, next_before?}` |
| `chat_session_read` | `id, last?(default 200, max 1000)` | `{session, turns}` |
| `chat_session_update` | `id, title?, pinned?` | `{rev}` |
| `chat_delete` | `id` | `{rev, deleted}` |
| `chat_search` | `query, from?, to?, kind?, limit?(20, max 100)` | `{hits:[{session_id, turn_id, at, snippet, score}]}` |
| `chat_sync` | `since_rev, limit?` | `{changes, rev, more, review_pending, reset}` |

Times are unix milliseconds from the host clock (client time is never
accepted); ids are ULIDs. `kind` is `chat` (never times out) or `voice`
(a new session starts after `CHATS_PAUSE_SECS` of silence; a `chat` session is always reused, with or without `session_id`). `status` is `ok`,
`failed` or `cancelled`.

## Revisions and sync

Every mutation takes the next value of a single host-wide counter
(`meta.rev`) inside the same transaction as the change. `chat_sync` returns
the changes with `rev > since_rev` in `(rev, op)` order (`session` before
`turn` before `delete`); a page never splits a group of changes sharing one
`rev`, so it may exceed `limit` slightly. Deletes are tombstones; after
`chat_delete` the session and its turns vanish from list, read, search and
sync (only the tombstone is synced).

## Search

The query is split into words; each becomes a quoted FTS5 prefix term and all
must match (AND), so FTS syntax characters in user input are harmless.
Matching is case-insensitive (including Cyrillic) and diacritics-insensitive.
`score` is `-bm25` (higher is better); `snippet` marks matches with `[` `]`.

## Events

After a mutation the plugin publishes `changed` (`plugin.chats.changed`) with
`{action}`; fetch details with `chat_sync`.

## Client notes

- Unknown request fields are rejected (`invalid params: unknown field ...`).
- `chat_sessions_list` hides sessions that have no turns (they still appear in
  `chat_sync`). When the reply has `next_before`, pass it back as `before`
  (opaque `<last_at>:<id>`; a bare integer `last_at` is still accepted).
- `chat_session_read` returns the newest 200 turns (ascending) unless `last` is
  given (max 1000).
- `tool_trace` must be an array; non-object entries are dropped and only
  `tool`, `args`, `result`, `error`, `ms` are kept.
- `chat_delete` is idempotent: it returns `deleted: true` only for the call that
  deleted the session; an unknown or already deleted id returns the current /
  tombstone `rev` with `deleted: false`. Deleting also blanks title, summary and tags.
- In `chat_sync`, a `turn` change can arrive before its `session` change (they
  share a revision only when written together; later session edits move the
  session to a newer revision). Clients should upsert a placeholder session for
  an unknown `session_id` and fill it when the `session` change arrives.
- `chat_sync` with `since_rev` greater than the store's current revision returns
  `reset: true`, `changes: []` and `rev` = the current revision: drop the local
  copy and resync from 0.
