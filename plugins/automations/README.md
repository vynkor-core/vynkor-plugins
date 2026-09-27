# automations plugin

Declarative rules engine (PLANS.md CAP-01): `trigger → [conditions] → action`
over the existing primitives. Rules are JSON documents in this plugin's own
`database` namespace (`rule:<id>`, atomic counter ids); triggers are kernel
event deliveries; actions are kernel-routed action calls.

## Actions

| Action | Params | Result |
|---|---|---|
| `rule_set` | `{id?, name?, enabled=true, trigger{event_type}, conditions?[], action{target_action, params_json}, requires_confirmation?, cooldown_ms?}` | `{id, stored}` |
| `rule_get` | `{id}` | `{found, rule?}` |
| `rule_list` | `{}` | `{total, rules}` |
| `rule_delete` | `{id}` | `{deleted}` |

- **Trigger** — fully-qualified event type; must be in the operator's
  `AUTOMATIONS_PLUGIN_EVENT_TYPES` (default-deny).
- **Conditions** — up to 8 JSON-pointer equality checks against the event
  payload (AND); empty = always fire.
- **Templating** — string values in `action.params_json` may hold
  `{{/json/pointer}}` placeholders resolved against the triggering event's
  payload. A string that is exactly one placeholder takes the pointed value
  with its JSON type (`null` on a miss); inside longer text placeholders are
  interpolated (strings verbatim, other values as JSON, a miss as `""`).
  Keys are never rewritten and nothing is evaluated. Rendered params are
  capped at 64 KiB (over-cap → `last_error`, no dispatch).
- **Cooldown** — `last_fired_ms` is marked BEFORE dispatch: at-most-once per
  window across restarts (calendar semantics).

## Confirmation gate

A rule with `requires_confirmation: true` never dispatches. It publishes
`plugin.automations.needs_confirmation {rule_id, name, action, …}` and stops;
the operator reviews and approves by `rule_set` with
`requires_confirmation: false` (same id). Decline = delete or disable.

## Events

Best-effort after every fire: `plugin.automations.triggered`
`{rule_id, name, event_type, ok}`; failed dispatches also land in the rule's
`last_error`.

## Cron pairing (no cron engine of its own)

```json
{"trigger": {"event_type": "plugin.scheduler.fired"},
 "conditions": [{"path": "/schedule_id", "equals": "3"}],
 "action": {"target_action": "goal_start", "params_json": {"goal": "brief me"}}}
```

## Example: alert that names the failing URL

```json
{"trigger": {"event_type": "plugin.uptime.check_failed"},
 "action": {"target_action": "notify_send",
            "params_json": {"title": "Site down",
                            "message": "{{/url}} failed {{/failures}} checks: {{/error}}",
                            "urgency": "critical"}}}
```

## Security

- Dispatch runs under **this plugin's** JWT grants — T-19 anti-laundering;
  gated targets need explicit operator grants, failures land in
  `last_error`.
- Subscription set is operator-declared; unknown event types never match.
- Templated values are event data: a rule inherits the trust of its
  trigger's source (e.g. a `telegram` message text is attacker-chosen).
- `params_json` cap 32 KiB (64 KiB after templating); ≤200 rules; conditions are exact-equality only
  (no scripting surface).
