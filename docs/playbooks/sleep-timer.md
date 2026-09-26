# PLAY-01: Sleep Timer — "podcast for 30 minutes"

Two primitives: `sound_play` + a `scheduler` one-shot that calls `sound_stop`.
No plugin code involved.

## 1. One-shot via `scheduler` (simplest)

```json
// start playback
{ "action": "sound_play", "params": {"file": "/home/user/podcast.mp3"} }

// stop it in 30 min
{
  "action": "schedule_set",
  "params": {
    "id": "sleep-30m",
    "name": "sleep timer",
    "once": {"delay_ms": 1800000},
    "action": {"name": "sound_stop", "params": {}}
  }
}
```

`sound_play` returns a `clip_id`; pass it as `{"clip_id": "..."}` to
`sound_stop` if other clips may be playing, otherwise `{}` stops everything.

For an MPRIS player (Spotify, mpv, browser) swap the stop action for
`{"name": "media_pause", "params": {}}` — omitting `player` pauses every
playing player.

## 2. Bound to a hotkey (reusable)

`automations` fires on kernel events only, so a "manual" trigger is a
`hotkey` binding. Requires `plugin.hotkey.hotkey_pressed` in
`AUTOMATIONS_PLUGIN_EVENT_TYPES`.

```json
{ "action": "hotkey_bind", "params": {"id": "sleep-timer", "trigger": "Super+Shift+S", "description": "sleep timer 30m"} }

{
  "action": "rule_set",
  "params": {
    "name": "sleep timer on Super+Shift+S",
    "trigger": {"event_type": "plugin.hotkey.hotkey_pressed"},
    "conditions": [{"path": "/binding", "equals": "sleep-timer"}],
    "action": {
      "target_action": "schedule_set",
      "params_json": {
        "id": "sleep-30m",
        "once": {"delay_ms": 1800000},
        "action": {"name": "media_pause", "params": {}}
      }
    }
  }
}
```

Reusing `id: "sleep-30m"` means pressing the key again restarts the 30-minute
window instead of stacking timers.

## 3. Via the agent

```bash
vyn ask "play /home/user/sleep.mp3 for 20 minutes then stop"
```

The agent maps "for N minutes" to `schedule_set` with `delay_ms = N*60000`
(needs `sound_play`, `sound_stop`, `schedule_set` in
`AGENT_PLUGIN_ALLOWED_ACTIONS`).

## Notes

- `schedule_set` → `sound_stop` runs under **scheduler's** grants (T-19);
  `sound_stop` is ungated, so no extra grant is needed.
- Cancel: `schedule_delete {"id": "sleep-30m"}`, or stop right away with
  `sound_stop {}`.
- `sound_play` takes local files or inline base64 only — no URLs. Streams
  go through an MPRIS player + `media_*`.
