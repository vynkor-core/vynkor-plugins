# PLAY-07: RSS digest — "what's new today"

`rss` has no timer of its own (pull only). A `scheduler` cron asks the agent
to fetch, summarize and deliver.

## Setup

```json
{"action": "rss_add", "params": {"url": "https://hnrss.org/frontpage"}}
{"action": "rss_add", "params": {"url": "https://lwn.net/headlines/rss"}}
```

### Plain refresh every hour (no LLM)

```json
{"action": "schedule_set", "params": {
  "id": "rss-refresh",
  "cron": {"expr": "0 * * * *", "tz": "Europe/Berlin"},
  "action": {"name": "rss_fetch_all", "params": {}}
}}
```

### Evening digest via the agent

```json
{"action": "schedule_set", "params": {
  "id": "rss-digest",
  "cron": {"expr": "0 19 * * *", "tz": "Europe/Berlin"},
  "action": {"name": "goal_start", "params": {
    "goal": "RSS digest: rss_fetch_all, then rss_articles with unread_only true (limit 30). Pick the 5 most interesting, one line each with the link. Send the digest with notify_send, then rss_mark_read each article you included.",
    "max_steps": 12
  }}
}}
```

Agent allowlist: `rss_fetch_all`, `rss_articles`, `rss_mark_read`,
`notify_send` (or `push_send` / `tg_send_message` for delivery elsewhere).

## On demand

```bash
vyn ask "what's new in my feeds today?"
```

## Notes

- `rss_mark_read` takes one id per call — marking 5 articles is 5 agent
  steps, hence `max_steps: 12`. A batch `rss_mark_read {ids[]}` is on
  `plugins/rss/ROADMAP.md`.
- Articles dedupe by link, so hourly refresh never duplicates.
