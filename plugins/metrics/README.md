# metrics plugin

Periodic host sampling for graphs and `vynkor-web` — CPU/RAM/disk/battery into the `database` (per-caller isolation like `vector-db`), queried by range.

## Actions

| Action | Params | Result |
|---|---|---|
| `metrics_query` | `{from_ms?, to_ms?, limit?, offset?}` | `{samples: [...], total}` — newest first |
| `metrics_latest` | — | `{found, sample?}` |
| `metrics_stats` | — | `{count, oldest_ms, newest_ms}` |
| `metrics_status` | — | `{version, uptime_ms, engine_ready}` |

Sample document:

```json
{
  "id": "1",
  "timestamp_ms": 1756320000000,
  "cpu_load_1": 1.2,
  "mem_total_kb": 16000000,
  "mem_available_kb": 8000000,
  "mem_used_percent": 50.0,
  "disk_total_bytes": 500000000000,
  "disk_available_bytes": 200000000000,
  "disk_used_percent": 60.0,
  "battery_percent": 80,
  "battery_charging": false
}
```

Every `METRICS_PLUGIN_INTERVAL_SECS` (default 30) the plugin samples `/proc/loadavg`, `/proc/meminfo`, `statvfs("/")`, `/sys/class/power_supply`, stores as `metric:<id>` and publishes `plugin.metrics.sample` `{id}` best-effort. Retention is a ring: `METRICS_PLUGIN_MAX_SAMPLES` (default 10000) oldest evicted.

### Threshold alerts

`METRICS_PLUGIN_ALERTS` holds rules like `battery_percent<15,disk_used_percent>90` over `cpu_load_1`, `mem_used_percent`, `disk_used_percent` and `battery_percent` (op `<` or `>`). Each sample is checked after it is stored:

- `plugin.metrics.threshold` `{metric, op, limit, value}` — once, when a metric crosses into breach.
- `plugin.metrics.threshold_cleared` (same shape) — once it is back past the limit by `METRICS_PLUGIN_ALERT_HYSTERESIS` (default 2, in the metric's own units), so a value wobbling around the limit doesn't flap.

A charging battery never counts as low, so plugging in clears a low-battery alert. A metric the host doesn't report (no battery) is skipped. Malformed rules are logged at startup and skipped; the rest apply. Alert state is in memory, so a restart during a breach alerts once more. Pair with an `automations` rule on `plugin.metrics.threshold` → `notify_send`.

## Config

| Env | Default | Meaning |
|---|---|---|
| `METRICS_PLUGIN_INTERVAL_SECS` | `30` | Sampling interval seconds |
| `METRICS_PLUGIN_MAX_SAMPLES` | `10000` | Ring cap, oldest trimmed |
| `METRICS_PLUGIN_DB_TIMEOUT_MS` | `5000` | DB IPC timeout |
| `METRICS_PLUGIN_ALERTS` | — | Threshold rules, see above |
| `METRICS_PLUGIN_ALERT_HYSTERESIS` | `2` | Clear margin, metric units |

## Testing

`cargo test` — sampler unit test + fake kernel e2e (query empty, stats empty, status ok); alert rule parsing and hysteresis.
