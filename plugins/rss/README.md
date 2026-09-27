# rss plugin

Feed reader as a thin schema over `database` + `network` — subscribe, fetch, and query articles.

Polling, not push — the plugin fetches on demand (`rss_fetch` / `rss_fetch_all`) or when asked by the agent/automation. No background timer here; schedule it via `scheduler` if needed.

## Actions

| Action | Params | Result |
|---|---|---|
| `rss_add` | `{url}` | `{id, feed}` |
| `rss_remove` | `{id}` | `{removed}` |
| `rss_list` | — | `{feeds: [...]}` |
| `rss_fetch` | `{id, timeout_ms?}` | `{fetched, new}` |
| `rss_fetch_all` | `{timeout_ms?}` | `{fetched, new, feeds}` |
| `rss_articles` | `{feed_id?, unread_only?, query?, limit?, offset?}` | `{articles: [...], total}` |
| `rss_mark_read` | `{id, read?}` | `{updated, article?}` |
| `rss_status` | — | `{version, uptime_ms, engine_ready}` |

- `url` must be `http://` or `https://`, 1..2048 bytes, trimmed.
- `timeout_ms` 1..30000, default `10000`.
- Articles are deduplicated by `link` (`article:<feed_id>:<hash(link)>`). `published_ms` is parsed from `pubDate`/`updated` when present.

Persistence: `feed:<id>` and `article:<id>` JSON docs in `database`, per-caller isolation (`db_*` RPC via `database` plugin). Uses `db_incr` for atomic id allocation.

Fetching routes through `network`'s `http_request` (declares `PERMISSION_NETWORK` for the `rss_fetch*` actions). The list/add paths are storage-only.

## Config

| Env | Default | Meaning |
|---|---|---|
| `RSS_PLUGIN_DB_TIMEOUT_MS` | `5000` | `database` IPC timeout |
| `RSS_PLUGIN_FETCH_TIMEOUT_MS` | `10000` | `network` HTTP timeout |

## Testing

`cargo test` — unit tests for URL validation + store round-trip + fake-kernel e2e (add → fetch with fixture RSS XML → articles query → mark_read) over `UnixStream::pair`. No live network required.
