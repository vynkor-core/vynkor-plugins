# PLAY-02: Smart Alarm — cron + rising volume + briefing

`scheduler` cron (IANA tz) → quiet `sound_play` → louder replay a bit later
→ agent briefing (weather + today's calendar, spoken).

## Why on the host

- Runs offline on the machine that owns the speakers (`sound` is their
  single owner).
- Soft wake: 10 % → 100 % in two steps, since `sound_play` replaces the
  current clip on every call.

## Setup: 07:30 Mon–Fri, Europe/Berlin

```json
// 1. 07:30:00 — quiet start (6-field cron, seconds first)
{
  "action": "schedule_set",
  "params": {
    "id": "alarm-soft",
    "cron": {"expr": "0 30 7 * * 1-5", "tz": "Europe/Berlin"},
    "action": {"name": "sound_play", "params": {"file": "/home/user/alarm.mp3", "volume": 0.1}}
  }
}

// 2. 07:30:45 — full volume (replaces the quiet clip)
{
  "action": "schedule_set",
  "params": {
    "id": "alarm-loud",
    "cron": {"expr": "45 30 7 * * 1-5", "tz": "Europe/Berlin"},
    "action": {"name": "sound_play", "params": {"file": "/home/user/alarm.mp3", "volume": 1.0}}
  }
}

// 3. 07:32 — spoken briefing through the agent
{
  "action": "schedule_set",
  "params": {
    "id": "alarm-briefing",
    "cron": {"expr": "0 32 7 * * 1-5", "tz": "Europe/Berlin"},
    "action": {"name": "goal_start", "params": {
      "goal": "Morning briefing: weather_forecast for lat 52.52 lon 13.40 (1 day, Europe/Berlin), today's events from event_list, then say it out loud via daemon_say. Keep it under 6 sentences."
    }}
  }
}
```

The scheduler scans every `SCHEDULER_PLUGIN_SCAN_SECS` (default 30), so
"07:30:45" lands within one scan of that instant — set the scan to `5` for
tighter ramps.

## What the agent does in step 3

1. `weather_forecast {"lat":52.52,"lon":13.40,"days":1,"timezone":"Europe/Berlin"}`
2. `event_list {"from_ms": <start of day>, "to_ms": <end of day>}`
3. `daemon_say {"text": "..."}` (or `tts_synthesize` → `sound_play`).

All three must be in `AGENT_PLUGIN_ALLOWED_ACTIONS`.

## Snooze / disable

- Snooze once: `schedule_set {"id":"alarm-snooze","once":{"delay_ms":300000},"action":{"name":"sound_play","params":{"file":"/home/user/alarm.mp3"}}}`
  — bind it to a hotkey via `automations` (see [sleep-timer.md](sleep-timer.md) §2).
- Stop ringing: `sound_stop {}`.
- Skip tomorrow: `schedule_set` the same ids with `"enabled": false`, re-enable after.
- Remove: `schedule_delete` for `alarm-soft`, `alarm-loud`, `alarm-briefing`.

## Notes

- Always use `tz`, not `tz_offset_min` — a fixed offset drifts by an hour
  across DST.
- A late fire after downtime carries `late: true` in `plugin.scheduler.fired`;
  one-shots fire at most once; a cron collapses downtime into at most one
  catch-up fire.
