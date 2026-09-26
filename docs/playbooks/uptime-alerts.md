# PLAY-06: Uptime alerts — "tell me when my site is down"

`uptime` scans its targets every `UPTIME_PLUGIN_INTERVAL_SECS` and publishes
`plugin.uptime.check_failed {url, status, error}` for every failing check
(background scan since `uptime@0.1.1`). An `automations` rule turns that
into a desktop notification or a phone push.

## Setup

```json
{"action": "uptime_add", "params": {"url": "https://example.com/health"}}
```

Add `plugin.uptime.check_failed` to `AUTOMATIONS_PLUGIN_EVENT_TYPES`, then:

```json
// desktop
{"action": "rule_set", "params": {
  "name": "example.com down → desktop",
  "trigger": {"event_type": "plugin.uptime.check_failed"},
  "conditions": [{"path": "/url", "equals": "https://example.com/health"}],
  "action": {"target_action": "notify_send", "params_json": {
    "title": "example.com is DOWN", "message": "Health check failed — see uptime_history", "urgency": "critical"
  }},
  "cooldown_ms": 900000
}}

// phone (ntfy/Gotify, see notify README "push_send")
{"action": "rule_set", "params": {
  "name": "example.com down → phone",
  "trigger": {"event_type": "plugin.uptime.check_failed"},
  "conditions": [{"path": "/url", "equals": "https://example.com/health"}],
  "action": {"target_action": "push_send", "params_json": {
    "title": "example.com is DOWN", "message": "Health check failed", "priority": 5
  }},
  "cooldown_ms": 900000
}}
```

`cooldown_ms` (15 min here) keeps a long outage from firing on every scan.
Drop `conditions` to alert on any target — the message then can't name the
URL, since rule params are static.

## Check what happened

```json
{"action": "uptime_history", "params": {"url": "https://example.com/health", "limit": 20}}
```

## Limits

- No recovery ("back up") event and no N-failures-in-a-row threshold yet —
  both on `plugins/uptime/ROADMAP.md`.
- `automations` params are static: the alert text cannot interpolate
  `status`/`error`. For a rich message, target `goal_start` with a goal
  that calls `uptime_history` and summarizes.
- Rule dispatch runs under **automations'** grants: `notify_send`/`push_send`
  need `PERMISSION_NOTIFY` granted to `automations`.
