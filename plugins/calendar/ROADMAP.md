# calendar plugin roadmap

v0.1 scope lives in `README.md`; this file tracks what's deliberately
deferred and what comes next.

## Implemented (v0.1)

- Event CRUD over `database`'s KV: `event:<id>` JSON documents, atomic id
  counter, chronological listing with inclusive start-time range filter /
  tag filter / pagination.
- Opt-in reminders (`remind_before_ms`): timer scan, at-most-once firing
  (mark-before-publish), `late` flag after downtime, fired-flag reset on
  reschedule.
- `plugin.calendar.changed` + `plugin.calendar.due` events; best-effort
  `notify_send` delivery.
- Channel-fronted RPC proxy so the serve loop stays the single reader of the
  connection (timer-driven scans must not eat user requests mid-flight).

## Non-goals (v1)

- **No recurrence rules (RRULE).** The biggest deferred feature; needs a
  storage-shape decision (expanded instances vs virtual occurrences) before
  any code.
- **No timezone conversion helpers.** Times are UTC unix-ms only; rendering
  and tz math belong to clients (web UI / agent prompts).
- **No multiple reminder leads per event** (e.g. day-before + 15-min).
  One opt-in lead covers v1 use cases; multiple leads want a per-event
  reminders array and a different fired-tracking shape.
- **No maximum-lateness cutoff window.** Overdue reminders always fire once,
  however old. If that gets noisy after long downtimes, add
  `CALENDAR_PLUGIN_MAX_LATE_MS` (skip-and-mark-stale past it).
- **No scheduler-plugin integration yet.** The scan loop is internal and
  self-contained. When the planned `scheduler` plugin ships, firing could
  migrate to it (one-shot schedule per reminder) — revisit then; until that
  decision, nothing here depends on it.

## Near-term ideas

- Recurrence rules (see above).
- ICS import/export actions for interop — **shipped (v0.2, 2026-08-26)**.
- `upcoming` convenience action (next N events across a horizon) — trivially
  composed client-side today via `event_list`, worth an action once agents
  need it often.
- Digest mode: one grouped notification per scan instead of one per reminder.
