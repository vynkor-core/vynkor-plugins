# notify plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next

- **Do-not-disturb** — `notify_dnd {until_ms | off}`: while active,
  `notify_send` stores silently (as `silent: true` does per call) and
  `urgency: critical` still breaks through. Focus mode (PLAY-03) then needs
  no caller cooperation.
- **Move speech playback to `sound`** — `speak: true` still resolves its own
  player; it should call `sound_play` so `sound` stays the single owner of
  the speakers (see `plugins/sound/ROADMAP.md`).
- **Actions/buttons** — `notify-send --action` → a
  `plugin.notify.action {id, action}` event for automations.
- **Inbox digest** — `notify_list {since_ms}` summary for the morning
  briefing.
