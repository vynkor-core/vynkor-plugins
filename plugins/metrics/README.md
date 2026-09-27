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

## Config

| Env | Default | Meaning |
|---|---|---|
| `METRICS_PLUGIN_INTERVAL_SECS` | `30` | Sampling interval seconds |
| `METRICS_PLUGIN_MAX_SAMPLES` | `10000` | Ring cap, oldest trimmed |
| `METRICS_PLUGIN_DB_TIMEOUT_MS` | `5000` | DB IPC timeout |

## Testing

`cargo test` — sampler unit test + fake kernel e2e (query empty, stats empty, status ok).
