# tasks plugin

To-do list as a thin schema over [`database`](../database/): every task is a
`task:<id>` JSON document in this plugin's own namespace, ids come from an
atomic counter. Callers need no storage permission — `tasks` holds it (T-19).

## Actions

| Action | Params | Result |
|---|---|---|
| `task_create` | `{title, notes?, list?, due_ms?, tags?}` | `{id, task}` |
| `task_get` | `{id}` | `{found, task?}` |
| `task_list` | `{query?, list?, status?, tag?, limit?, offset?}` | `{tasks, total}` |
| `task_update` | `{id, title?, notes?, list?, due_ms?, tags?, done?}` | `{updated, task}` |
| `task_done` | `{id, done?}` | `{done, task}` — `done: false` reopens |
| `task_delete` | `{id}` | `{deleted}` |
| `tasks_status` | — | `{version, uptime_ms, engine_ready, …}` |

`task_list`:

- `status` — `done`, `all`, or anything else (e.g. `pending`) for open tasks;
  omitted = all.
- `list` — exact list name (tasks default to the list `default`).
- `tag` — exact tag; `query` — case-insensitive substring over title + notes.
- Sorted by `updated_at_ms` descending; `limit` 1..500 (default 100).

## Validation

- `title` required, ≤ 512 bytes; `notes` ≤ 4096; `list` ≤ 64.
- `tags` ≤ 32, each ≤ 64 bytes.
- `due_ms` is stored and returned but nothing acts on it yet — reminders
  are the first item on [`ROADMAP.md`](ROADMAP.md).

## Events

`plugin.tasks.changed` `{op: created|updated|completed|reopened|deleted, id}`,
published best-effort after the response.

## Config

| Env | Default | Meaning |
|---|---|---|
| `TASKS_PLUGIN_DB_TIMEOUT_MS` | `5000` | `database` IPC timeout |

## Permissions

`PERMISSION_STORAGE`, `PERMISSION_EVENT_PUBLISH`.

## Testing

`cargo test --manifest-path plugins/tasks/Cargo.toml` — fake-kernel e2e over
`UnixStream::pair` (CRUD, status filters, `task_done`, limit validation) and
a manifest check that every declared action has a spec.
