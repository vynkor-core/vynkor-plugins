# tasks plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next

- **Due reminders** — `remind_before_ms` like `calendar`, fired through
  `scheduler` (one-shot per task) → `plugin.tasks.due` + `notify_send`.
  `due_ms` is stored today but nothing acts on it.
- **Priority + recurrence** — `priority` (1..4), `rrule` reusing
  `calendar`'s RRULE code; completing a recurring task spawns the next one.
- **Subtasks / checklist items.**
- **Google Tasks sync** (INT-19 remainder) — after the shared OAuth crate
  (ARCH-02).
- **STAT-01** — `status` → `tasks_status`.
