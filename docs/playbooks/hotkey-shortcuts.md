# CLI-07: Hotkey shortcuts — any key combo → any action

`hotkey` publishes `plugin.hotkey.hotkey_pressed {binding}`; an
`automations` rule filtered on `/binding` dispatches one action. This is the
"manual trigger" for every other playbook.

Prerequisite: `plugin.hotkey.hotkey_pressed` in
`AUTOMATIONS_PLUGIN_EVENT_TYPES`.

## Pattern

```json
{"action": "hotkey_bind", "params": {"id": "<binding>", "trigger": "Super+Shift+<Key>", "description": "..."}}

{"action": "rule_set", "params": {
  "name": "<binding> → <action>",
  "trigger": {"event_type": "plugin.hotkey.hotkey_pressed"},
  "conditions": [{"path": "/binding", "equals": "<binding>"}],
  "action": {"target_action": "<action>", "params_json": {}}
}}
```

## Ready-made bindings

| Binding | Trigger | target_action | params_json |
|---|---|---|---|
| `media-toggle` | `Super+Shift+P` | `media_play_pause` | `{}` |
| `lock` | `Super+Shift+L` | `sys_lock` | `{}` |
| `mute` | `Super+Shift+M` | `sys_volume_mute` | `{"mode": "toggle"}` |
| `briefing` | `Super+Shift+B` | `goal_start` | `{"goal": "Brief me: weather, today's events, unread notify_list items. Say it via daemon_say."}` |
| `listen` | `Super+Shift+Space` | `daemon_turn` | `{}` |
| `stop-audio` | `Super+Shift+X` | `sound_stop` | `{}` |

Push-to-talk for the voice daemon does not need a rule — `daemon` subscribes
to the hotkey events itself (`DAEMON_PLUGIN_MODE=ptt`, see its README).

## Notes

- Every binding needs at least one modifier (bare keys are refused).
- On Wayland the XDG GlobalShortcuts portal asks the user to confirm new
  bindings once.
- Gated targets (`sys_lock`, `goal_start`, …) run under **automations'**
  grants — give `automations` the permission the target declares.
