# github plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Shipped (0.1.0)

`gh_list_issues`, `gh_create_issue`, `gh_list_prs`, `gh_list_runs` — REST
via `network`, vault-first PAT.

## Next

- **Read detail** — `gh_get_issue` / `gh_get_pr` (body, labels, review state,
  checks) and `gh_get_run` with failed job names + log tail, so "why did CI
  fail" is answerable.
- **Write** — `gh_comment`, `gh_close_issue`, `gh_label` (`risk: medium`);
  `gh_merge_pr` and `gh_rerun_run` behind `requires_confirmation`.
- **Notifications** — `gh_notifications {unread}` for the morning briefing.
- **Releases** — `gh_list_releases`, latest release per repo.
- Polling event (`plugin.github.run_failed`) for a CI-watch playbook —
  outbound polling only, no webhooks (no inbound ports by design).
