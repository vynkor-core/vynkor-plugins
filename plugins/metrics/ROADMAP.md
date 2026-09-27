# metrics plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next (EXI-11)

- **Threshold events** — operator rules in env, e.g.
  `METRICS_PLUGIN_ALERTS=battery_percent<15,disk_used_percent>90`,
  publishing `plugin.metrics.threshold {metric, value, limit}` once per
  crossing (hysteresis, not every sample). Unblocks a low-battery / disk-full
  playbook through `automations` → `notify` with zero new plugins.
- **More signals** — CPU % (from `/proc/stat` deltas, not just loadavg),
  network rx/tx bytes/s, temperatures (`/sys/class/thermal`), per-mount disk.
- **Downsampling** — `metrics_query {bucket_ms}` returning min/avg/max per
  bucket so a week of 30 s samples renders as ~300 points.
