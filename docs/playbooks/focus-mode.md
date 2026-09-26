# PLAY-03: Focus Mode — "do not disturb for an hour"

`media_pause` + a timed "focus over" notification. Callers that should stay
quiet during focus send with `silent: true`, which stores the notification
in `notify`'s inbox without delivering it (`notify_list` reads it later).

> `notify` has no global do-not-disturb switch yet — silence is per call.
> A `notify_dnd {until_ms}` action is on `plugins/notify/ROADMAP.md`.

## Manual

```json
// 1. pause every playing MPRIS player
{"action": "media_pause", "params": {}}

// 2. end of focus in 60 min
{
  "action": "schedule_set",
  "params": {
    "id": "focus-end",
    "name": "focus over",
    "once": {"delay_ms": 3600000},
    "action": {"name": "notify_send", "params": {"title": "Focus", "message": "Focus mode over — check notify_list for what arrived"}}
  }
}
```

## On a hotkey (recommended)

Needs `plugin.hotkey.hotkey_pressed` and `plugin.scheduler.fired` in
`AUTOMATIONS_PLUGIN_EVENT_TYPES`. One rule per action, since a rule
dispatches exactly one action.

```json
{"action": "hotkey_bind", "params": {"id": "focus", "trigger": "Super+Shift+F", "description": "focus 60m"}}

// press → pause media
{"action": "rule_set", "params": {
  "name": "focus: pause media",
  "trigger": {"event_type": "plugin.hotkey.hotkey_pressed"},
  "conditions": [{"path": "/binding", "equals": "focus"}],
  "action": {"target_action": "media_pause", "params_json": {}}
}}

// press → arm the 60 min timer (event mode: publishes plugin.scheduler.fired)
{"action": "rule_set", "params": {
  "name": "focus: arm timer",
  "trigger": {"event_type": "plugin.hotkey.hotkey_pressed"},
  "conditions": [{"path": "/binding", "equals": "focus"}],
  "action": {"target_action": "schedule_set", "params_json": {
    "id": "focus-end", "once": {"delay_ms": 3600000}, "event": {"payload": {"focus": "off"}}
  }}
}}

// timer fired → resume media
{"action": "rule_set", "params": {
  "name": "focus: resume media",
  "trigger": {"event_type": "plugin.scheduler.fired"},
  "conditions": [{"path": "/schedule_id", "equals": "focus-end"}],
  "action": {"target_action": "media_play", "params_json": {}}
}}

// timer fired → announce
{"action": "rule_set", "params": {
  "name": "focus: announce end",
  "trigger": {"event_type": "plugin.scheduler.fired"},
  "conditions": [{"path": "/schedule_id", "equals": "focus-end"}],
  "action": {"target_action": "notify_send", "params_json": {"title": "Focus", "message": "Focus mode over"}}
}}
```

## Via the agent

```bash
vyn ask "do not disturb for an hour, pause the music"
```

## Cancel early

```json
{"action": "schedule_delete", "params": {"id": "focus-end"}}
{"action": "media_play", "params": {}}
```
