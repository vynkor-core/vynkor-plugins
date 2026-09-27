# uptime plugin

Health-monitor for URLs — periodic `http_request` via `network`, history in `database`, `check_failed` events for `automations`/`notify`.

## Actions

| Action | Params | Result |
|---|---|---|
| `uptime_add` | `{url}` | `{id, target}` — idempotent by URL |
| `uptime_remove` | `{id}` | `{removed}` |
| `uptime_list` | — | `{targets: [...]}` |
| `uptime_check` | `{url, timeout_ms?}` | `{url, ok, status, latency_ms, error}` — on-demand check, also stored as `check:<id>` |
| `uptime_history` | `{url?, limit?, offset?}` | `{checks: [...], total}` — newest first |
| `uptime_status` | — | `{version, uptime_ms, engine_ready}` |

Target: `{id, url, created_at_ms}` stored as `target:<id>`. Check: `{id, url, timestamp_ms, ok, status, latency_ms, error}` as `check:<id>`. Ring cap `UPTIME_PLUGIN_MAX_CHECKS` (default 5000) oldest trimmed.

Background scan every `UPTIME_PLUGIN_INTERVAL_SECS` (default 60) checks all targets via `network` `http_request` (follow_redirects), stores checks, trims. Alerts are per outage, not per check:

- `plugin.uptime.check_failed` `{url, status, error, failures}` — once, when a target has failed `UPTIME_PLUGIN_FAIL_THRESHOLD` scans in a row (default 2, so a single timeout pages nobody).
- `plugin.uptime.recovered` `{url, down_for_ms, failures}` — on the first OK scan after that alert; `down_for_ms` counts from the first failed check.

A failure streak that heals before the threshold publishes neither. Outage state lives in `state:<target_id>` and is dropped by `uptime_remove`. An on-demand `uptime_check` still publishes `check_failed` immediately (no threshold) and does not touch the outage state.

## Config

| Env | Default | Meaning |
|---|---|---|
| `UPTIME_PLUGIN_INTERVAL_SECS` | `60` | Scan interval seconds (min 5) |
| `UPTIME_PLUGIN_MAX_CHECKS` | `5000` | Ring cap |
| `UPTIME_PLUGIN_DB_TIMEOUT_MS` | `5000` | DB IPC timeout |
| `UPTIME_PLUGIN_CHECK_TIMEOUT_MS` | `5000` | Per-check http timeout |
| `UPTIME_PLUGIN_FAIL_THRESHOLD` | `2` | Consecutive failed scans before `check_failed` (min 1) |

## Testing

`cargo test` — fake kernel e2e over `UnixStream::pair`: add/list/remove, check 200, history empty, status; outage state machine (threshold, single alert, recovery, removal).
