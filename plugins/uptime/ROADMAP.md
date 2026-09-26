# uptime plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Shipped

- 0.1.0 — targets, on-demand checks, background scan, history ring.
- 0.1.1 — the background scan publishes `check_failed` (previously only
  on-demand `uptime_check` did, so scheduled monitoring never alerted).

## Next

- **Recovery event** — `plugin.uptime.recovered {url, down_for_ms}` on the
  first OK after a failure, so alerts can close themselves (needs the last
  state per target persisted).
- **Flap guard** — `UPTIME_PLUGIN_FAIL_THRESHOLD` (N consecutive failures
  before `check_failed`); today a single timeout alerts.
- **Per-target options** — `expect_status`, `keyword` (body must contain),
  `max_latency_ms` (slow counts as failed), own interval.
- **`uptime_summary {url?, window_ms}`** — uptime %, p50/p95 latency for
  briefings and the web dashboard.
- **STAT-01** — `status` → `uptime_status`.
