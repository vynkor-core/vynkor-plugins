# chats

Host-wide conversation history for every device: sessions, turns, full-text
search (SQLite FTS5) and revision-based sync. One SQLite file
(`$CHATS_DATA_DIR/chats.db`) per host; history is shared across callers.

Status: C2 (semantic layer). A background worker writes session summaries,
titles and tags through `ai`, stores vectors in `vector-db`, search is hybrid
(FTS5 + vectors, reciprocal rank fusion), and deleting a session cascades to
vectors, agent goals and agent facts. Compaction arrives later.

## Configuration (environment)

| Variable | Default | Meaning |
|---|---|---|
| `CHATS_DATA_DIR` | required | directory holding `chats.db` |
| `CHATS_PAUSE_SECS` | 1800 | silence after which a voice session ends |
| `CHATS_TRACE_MAX_CHARS` | 200 | tool_trace `args`/`result`/`error` cut to this |
| `CHATS_TEXT_MAX_BYTES` | 65536 | `user_text`/`answer_text` cut to this (char boundary, `…` suffix) |
| `CHATS_SYNC_MAX` | 500 | upper bound for a `chat_sync` page |
| `CHATS_SEMANTIC` | on | `on`/`off`; off = plain full-text behaviour, no calls to other plugins |
| `CHATS_WORK_INTERVAL_SECS` | 60 | background worker tick (it is also woken after every append/delete) |
| `CHATS_SUMMARY_IDLE_SECS` | 900 | a session is summarized once silent this long |
| `CHATS_AI_AGENT_ID` | unset | optional `ai` agent profile used for summaries |
| `CHATS_SUMMARY_MAX_TOKENS` | 400 | token cap of a summary completion |
| `CHATS_SEARCH_VEC_TIMEOUT_MS` | 800 | budget for the vector half of `chat_search`; on timeout only text hits are returned |
| `CHATS_RECALL_MIN_SCORE` | 0.72 | default `min_score` of `chat_recall` |

## Actions

| Action | Params | Returns |
|---|---|---|
| `chat_session_resolve` | `device, kind, source?, session_id?` | `{session_id, created}` |
| `chat_turn_append` | `session_id, user_text, answer_text, status, tool_trace?, goal_id?` | `{turn_id, seq, rev}` |
| `chat_sessions_list` | `from?, to?, kind?, source?, limit?(50, max 200), before?(cursor)` | `{sessions, next_before?}` |
| `chat_session_read` | `id, last?(default 200, max 1000)` | `{session, turns}` |
| `chat_session_update` | `id, title?, pinned?` | `{rev}` |
| `chat_delete` | `id` | `{rev, deleted}` |
| `chat_search` | `query, from?, to?, kind?, limit?(20, max 100)` | `{hits:[{session_id, turn_id \| null, at, snippet, score}]}` |
| `chat_recall` | `vector, model, exclude_session_id?, top_k?(3, max 10), min_score?` | `{hits:[{session_id, title, summary, at, score}]}` |
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

## Semantic layer (C2)

Needs the `ai`, `vector-db` and `agent` plugins; while one is not available
(`action not found`) its work is skipped with one warning and `chats` keeps
working like C1. The exception is the delete cascade: a missing plugin may be
restarting, so its steps stay queued and are retried indefinitely.

- **Summaries.** A live session silent for `CHATS_SUMMARY_IDLE_SECS` whose
  `rev` moved since the last summary gets one `chat_completion` (JSON title,
  summary, tags). The result is written with a new `rev` (so `chat_sync`
  clients see it); a title set by `chat_session_update` is never overwritten.
  An unusable reply marks that `rev` as processed (no retry loop, nothing
  overwritten); the next turn makes the session a candidate again.
- **Vectors.** Turns of `tier = full` sessions go to `chats-turns`
  (`"Q: ...\nA: ..."`, <= 2000 chars), summaries to `chats-sessions`
  (`title\nsummary`), metadata `{session_id, kind, at}`.
- **Hybrid `chat_search`.** FTS top 50 plus vector top 50 over turns and
  summaries, merged by reciprocal rank fusion (`score = sum 1/(60+rank)`).
  Vector hits of deleted sessions are dropped; a slow or failing
  `vector-db` degrades to the FTS result inside `CHATS_SEARCH_VEC_TIMEOUT_MS`.
  A hit on a session summary has `turn_id: null`. With `CHATS_SEMANTIC=off`
  the C1 scoring (`-bm25`) is used.
- **`chat_recall`.** Similar past sessions for a ready-made vector (used by
  the agent). `model` must equal the model of the `chats-sessions` collection
  (`vec_stats`, cached 60 s); a different model or `"fake"` returns no hits.
- **Delete cascade.** `chat_delete` queues a job in the same transaction. The
  worker deletes the turn and session vectors, calls `goal_delete` per
  `goal_id` and `memory_forget_session` on `agent`, retrying with backoff
  (1 min, doubled, max 1 h, no attempt limit; finished steps are not repeated;
  a plugin that is missing counts as a failed step, never as done). When done the
  session row is removed (the tombstone stays). A daily sweep repeats the
  vector deletes for sessions finished in the last 7 days to catch late writes.

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
