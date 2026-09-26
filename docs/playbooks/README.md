# Playbooks — zero-code compositions

Every playbook is a composition of shipped plugins through `scheduler`,
`automations`, `hotkey` and `agent` — no plugin code. Examples are checked
against the manifests' `input` schemas; if a playbook and a manifest
disagree, the manifest wins — fix the playbook.

| ID | Playbook | File | Primitives |
|---|---|---|---|
| PLAY-01 | Sleep timer | [sleep-timer.md](sleep-timer.md) | `sound_play` + `scheduler` one-shot → `sound_stop` / `media_pause` |
| PLAY-02 | Smart alarm | [smart-alarm.md](smart-alarm.md) | `scheduler` cron (IANA tz) → `sound_play` ×2 → `goal_start` briefing |
| PLAY-03 | Focus mode | [focus-mode.md](focus-mode.md) | `hotkey` → `automations` → `media_pause` + `scheduler` → `media_play` |
| PLAY-04 | Voice DJ | [voice-dj.md](voice-dj.md) | `agent` / `library_search` → `sound_play` or `media_*` |
| PLAY-06 | Uptime alerts | [uptime-alerts.md](uptime-alerts.md) | `uptime` `check_failed` → `automations` → `notify_send` / `push_send` |
| PLAY-07 | RSS digest | [rss-digest.md](rss-digest.md) | `scheduler` cron → `goal_start` → `rss_*` + `notify_send` |
| AGT-08 | Scheduled goals | [scheduled-goals.md](scheduled-goals.md) | `scheduler` cron/one-shot → `goal_start` |
| CLI-07 | Hotkey shortcuts | [hotkey-shortcuts.md](hotkey-shortcuts.md) | `hotkey_bind` + `rule_set` on `hotkey_pressed` → any action |

## Common prerequisites

- **`automations` subscriptions are default-deny.** Every event a rule
  triggers on must be listed in `AUTOMATIONS_PLUGIN_EVENT_TYPES`, e.g.
  `plugin.hotkey.hotkey_pressed,plugin.scheduler.fired,plugin.uptime.check_failed,plugin.calendar.due`.
- **Dispatch runs under the dispatcher's grants (T-19).** A `scheduler` or
  `automations` action call to a gated target (`notify_send`, `sys_lock`,
  `goal_start`, …) needs that permission granted to `scheduler` /
  `automations`, not to the caller who set the rule up.
- **Agent goals need an allowlist.** Every tool a goal uses must be in
  `AGENT_PLUGIN_ALLOWED_ACTIONS`.
- **Rule params are static.** `automations` cannot template event fields
  into `params_json`; route through `goal_start` when the reaction must
  depend on the payload.

## Ideas not written yet

PLAY-05 language tutor (persona-pack for `agent` + `speech`), a
"leaving home" macro (`sys_lock` + `media_pause` + `notify` silent), and a
low-battery warning — the last one needs a threshold event from `metrics`
first (`plugins/metrics/ROADMAP.md`).
