# chats

Host-wide conversation history for every device: sessions, turns, full-text
search (SQLite FTS5) and revision-based sync. One SQLite file
(`$CHATS_DATA_DIR/chats.db`) per host; history is shared across callers.

Status: C2 (semantic layer). A background worker writes session summaries,
titles and tags through `ai`, stores vectors in `vector-db`, search is hybrid
(FTS5 + vectors, reciprocal rank fusion), and deleting a session cascades to
vectors, agent goals and agent facts. Retention (C3) keeps the store tidy: usage tracking, automatic junk
cleanup, compaction of old unused sessions and a monthly review for the owner.

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
| `CHATS_RETENTION` | on | `on`/`off`: junk cleanup, compaction, monthly review (runs in the worker, so it needs `CHATS_SEMANTIC=on`) |
| `CHATS_MAINT_INTERVAL_SECS` | 3600 | maintenance pass period |
| `CHATS_JUNK_AGE_SECS` | 86400 | a session of only junk turns is deleted once this old |
| `CHATS_COMPACT_AGE_SECS` | 7776000 | sessions older than this (and summarized) may be compacted |
| `CHATS_COMPACT_UNUSED_SECS` | 5184000 | ... if also not found by search/recall for this long |
| `CHATS_REVIEW_INTERVAL_SECS` | 2592000 | how often a review is prepared |
| `CHATS_REVIEW_LOWVALUE_AGE_SECS` | 2592000 | review `junk` group: low-value voice sessions older than this |
| `CHATS_REVIEW_SUMMARY_AGE_SECS` | 15552000 | review `compress` group: compact sessions unused this long |

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
| `chat_review_get` | - | `{review: null \| {id, created_at, groups:[{key, action, sessions:[{id, title, last_at, turns, tier}]}]}}` |
| `chat_review_apply` | `id, accept:[key], exclude?:[session_id]` | `{deleted, compressed, skipped, rev}` |
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
- **Worker lanes.** Summaries and embeddings run in one lane, the delete
  cascade and sweep in another, so a hung `ai` never delays deletions. A cascade
  or sweep pass has a 60 s budget and stops calling a plugin after its first
  failure. A turn that `vector-db` rejects on its own is retried 3 times and
  then skipped.
- **Hybrid `chat_search`.** The query is embedded once (`vec_embed`, falling
  back to text). FTS top 50 plus vector top 50 over turns and
  summaries, merged by reciprocal rank fusion (`score = sum 1/(60+rank)`).
  Vector hits of deleted sessions are dropped; a slow or failing
  `vector-db` degrades to the FTS result inside `CHATS_SEARCH_VEC_TIMEOUT_MS`.
  A hit on a session summary has `turn_id: null`. Known limit: `from`/`to`
  are applied after the vector query, so a narrow range can return fewer vector hits. With `CHATS_SEMANTIC=off`
  the C1 scoring (`-bm25`) is used.
- **`chat_recall`.** Similar past sessions for a ready-made vector (used by
  the agent). `model` must equal the model of the `chats-sessions` collection
  (`vec_stats`, cached 60 s); a different model or `"fake"` returns no hits.
- **Delete cascade.** `chat_delete` queues a job in the same transaction. The
  worker deletes the turn and session vectors, calls `goal_delete` per
  `goal_id` and `memory_forget_session` on `agent`, retrying with backoff
  (1 min, doubled, max 1 h, no attempt limit; finished steps are not repeated;
  a plugin that is missing counts as a failed step, never as done). When done the
  session row is removed (the tombstone stays). A daily sweep cycle repeats the
  vector deletes for sessions finished in the last 7 days to catch late writes;
  it keeps its place across partial passes. A requeue (late vector found by the
  worker) bumps the job's generation so a concurrently finishing cascade cannot
  drop it.

## Retention (C3)

Tiers: `full` -> `compact` (automatic) -> `summary` (only by the owner's
review). `pinned` sessions are never touched by any rule, nor by a review
applied after they were pinned.

- **Usage.** `chat_recall` hits count `recall_count` and set `last_used_at`;
  `chat_search` hits set `last_used_at`. Host-local, never bumps `rev`.
- **Junk.** A live, unpinned session older than `CHATS_JUNK_AGE_SECS` whose
  every turn is junk (status not `ok`, or an empty question/answer; or no
  turns at all) is deleted through the normal delete path (tombstone and
  cascade). One good turn keeps the session.
- **Compaction.** A summarized `full` session older than
  `CHATS_COMPACT_AGE_SECS` and unused for `CHATS_COMPACT_UNUSED_SECS` becomes
  `compact`: tool traces are emptied (new `rev`), turn vectors and agent goals
  are removed through a `cascade` job in `compact` mode (retried like a delete;
  summary, tags, summary vector and facts stay). A new turn makes the session
  `full` again.
- **Monthly review.** Every `CHATS_REVIEW_INTERVAL_SECS` the worker prepares a
  review with two groups: `junk` (low-value voice sessions: at most one good
  turn, never recalled, older than `CHATS_REVIEW_LOWVALUE_AGE_SECS`; action
  delete) and `compress` (compact sessions unused for
  `CHATS_REVIEW_SUMMARY_AGE_SECS`; action summary = drop the turns, keep title,
  summary and tags, `tier = summary`, no tombstones). `chat_sync` reports
  `review_pending`; the worker publishes `changed` with `action: "review"`.
  `chat_review_apply` carries out the accepted group keys, re-checking every
  session first (pinned, used or deleted since -> `skipped`). A review can be
  applied once (`review already applied`); an unknown id is `review not found`.

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
