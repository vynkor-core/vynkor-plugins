# metrics plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Shipped

- 0.1.1 — `metrics_status` (was a bare `status`, unroutable — STAT-01);
  threshold alerts (EXI-11): `METRICS_PLUGIN_ALERTS` rules publish
  `threshold` / `threshold_cleared` once per crossing, with hysteresis.

## Next

- **More signals** — CPU % (from `/proc/stat` deltas, not just loadavg),
  network rx/tx bytes/s, temperatures (`/sys/class/thermal`), per-mount disk.
- **Downsampling** — `metrics_query {bucket_ms}` returning min/avg/max per
  bucket so a week of 30 s samples renders as ~300 points.
