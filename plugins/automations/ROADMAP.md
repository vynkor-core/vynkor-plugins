# automations plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next

- **Payload templating** — `params_json` values like `"{{/url}}"` resolved
  from the triggering event (JSON pointer only, no expressions). Today rule
  params are static, so an alert can't say *which* URL is down without
  routing through the agent.
- **Richer conditions** — `not_equals`, `exists`, numeric `gt`/`lt`, `in`,
  and OR groups; still no scripting surface.
- **Multi-action rules** — ordered `actions[]`, so a playbook like focus
  mode is one rule instead of four.
- **`rule_test {id, payload}`** — dry-run: evaluate conditions and show the
  resolved action without dispatching.
- **Fire log** — `rule_history {id}` (last N fires with ok/error), beyond
  the single `last_error`.
- **`rule_enable {id, enabled}`** convenience (today: full `rule_set`).
