# daemon plugin — operator & caller guide

Everything a caller (or the operator's config) needs to drive the `daemon`
plugin. Actions: `daemon_enable`, `daemon_disable`, `daemon_status`,
`daemon_turn`, `daemon_say`, `daemon_ask`.

## Prerequisites

A full answered turn needs all five downstream plugins registered:

```
mic → stt → agent → tts → sound
```

- `stt` must have its local sherpa model configured (the listen path is
  local-only).
- `agent` needs its LLM routing set (`AGENT_PLUGIN_AI_*` or per-goal
  overrides) and an operator allowlist for any tools it may dispatch.
- `tts` needs a working provider — `sherpa` is fully offline.
- The mic→stt hop is mic streaming to the slug `stt` as `AudioStreamChunk`s;
  nothing extra is needed on the daemon side, but the kernel's T-04
  allowlist for *mic* (`MIC_PLUGIN_IPC_TARGETS`) must contain `stt`.

Missing plugins degrade per-stage: the turn completes with
`status: "error"` naming the failed stage; `daemon_say`/`daemon_ask` return
ACTION_ERROR.

## Actions

### `daemon_say`

Synthesize text and play it. No listening.

```json
{ "text": "Backup finished." }
```

Response (returned once the player is spawned — playback continues in the
background):

```json
{ "spoken": true, "clip_id": "clip-1", "player": "pw-cat", "format": "wav" }
```

Use `clip_id` with `sound_stop` to cut playback short. Provider/voice/format
come from config (`DAEMON_PLUGIN_TTS_*`); this action takes no overrides in
v0.1 — call `tts` directly when you need them.

Errors: `ERR_DAEMON_BAD_PARAMS: missing required field: text`,
`ERR_DAEMON_BAD_PARAMS: non-empty text required`, `text exceeds 4000 chars`,
or the failing stage (`tts_synthesize failed: …`, `sound_play failed: …`).

### `daemon_ask`

One agent round-trip, spoken aloud.

```json
{ "prompt": "What's on my calendar in the next hour?" }
```

```json
{
  "answer": "Dentist at 14:00, then free.",
  "goal_id": "7",
  "goal_status": "completed",
  "spoken": true
}
```

`answer` is null unless `goal_status` is `completed` — a goal that halts for
confirmation (`needs_confirmation`) or hits the step budget is reported, not
spoken. Resume confirmation-gated goals through `agent`'s own
`goal_resume`; the daemon never approves tools on anyone's behalf.

Errors: malformed prompt (see `daemon_say`), `goal_start failed: …` (agent
or its `ai` dependency down), `tts_synthesize/sound_play failed: …`.

### `daemon_turn`

Run one voice cycle now — the background loop's unit of work, callable by
hand (and the only way to use voice turns while the loop is disabled).

```json
{}
```

…listens per the configured mode (see Configuration) and processes whatever
it hears. With a `text` override the mic/stt stages are skipped entirely:

```json
{ "text": "summarize my notes" }
```

```json
{
  "status": "answered",
  "transcript": "what time is it",
  "answer": "It is noon.",
  "goal_id": "7",
  "goal_status": "completed",
  "spoken": true,
  "duration_ms": 8120,
  "error": null
}
```

| status | meaning |
|---|---|
| `answered` | transcript → goal completed → answer spoken |
| `silent` | empty/whitespace transcript after the endpoint; no agent/tts/sound calls were made |
| `error` | a stage failed; `error` names it (e.g. `"mic_start failed: …"`) |

Always ACTION_OK — check `status`. While another turn is running:
ACTION_ERROR `ERR_DAEMON_BUSY: another voice turn is already in progress`.

### `daemon_enable` / `daemon_disable`

Toggle the background loop. Both take `{}` and answer
`{"enabled": true|false}`, publishing `state.changed`. Disable lets an
in-flight turn finish naturally; ticks are skipped while busy, so no turn is
ever cut off mid-capture. The loop starts **off** unless
`DAEMON_PLUGIN_ENABLED=true` at boot — an always-on mic is an opt-in.

### `daemon_status`

```json
{ "enabled": false, "busy": false, "capturing": false, "mode": "window",
  "turns_completed": 3,
  "last_turn": { "status": "answered", "transcript": "…", "answer": "…",
                 "duration_ms": 8120 } }
```

`last_turn` is null before the first turn. `daemon_say` does not count as a
turn. `mode` echoes `DAEMON_PLUGIN_MODE`; `capturing` is true while the
mic is actually open (vad/ptt hold it open-endedly).

## Listen modes

| Mode | Endpoint | Needs | Feels like |
|---|---|---|---|
| `window` | fixed `TURN_MS` elapses | nothing | walkie-talkie with a timer |
| `vad` | stt's speech-ended event (silence after real speech), capped by wait/utterance budgets | `STT_PLUGIN_VAD=on` on stt | hands-free: enable once, talk whenever |
| `ptt` | hotkey released (`DAEMON_PLUGIN_PTT_BINDING`) | `hotkey` plugin registered + binding | push-to-talk |

All modes feed the same turn pipeline; switch via env, no code changes.
In vad mode a turn that hears no speech within `VAD_WAIT_MS` ends
`silent`; an utterance that never ends within `VAD_MAX_UTTERANCE_MS` is cut
off and transcribed anyway. In ptt mode a key held past
`PTT_MAX_HOLD_MS` auto-releases and the turn reports `error`
("hotkey release never arrived").

## Events

Subscribers receive (namespaced by the kernel):

- `plugin.daemon.turn.completed` — payload identical to `daemon_turn`'s
  response, for every finished turn regardless of who started it.
- `plugin.daemon.state.changed` — `{"enabled": true}` / `{"enabled": false}`.

## Configuration

Environment variables read from the plugin's `env:` list (kernel
config.yaml / drop-in); see `config.example.yaml`.

| Env var | Default | Meaning |
|---|---|---|
| `DAEMON_PLUGIN_ENABLED` | `false` | start with the listen loop on |
| `DAEMON_PLUGIN_MODE` | `window` | `window` \| `vad` \| `ptt` — see Listen modes above |
| `DAEMON_PLUGIN_TURN_MS` | `6000` | window mode: mic capture window per turn (100–120000); manual turns in ptt mode also use it |
| `DAEMON_PLUGIN_VAD_WAIT_MS` | `30000` | vad mode: max silence before any speech (1000–600000) |
| `DAEMON_PLUGIN_VAD_MAX_UTTERANCE_MS` | `20000` | vad mode: hard cap on one utterance (500–120000) |
| `DAEMON_PLUGIN_PTT_BINDING` | `ptt` | ptt mode: hotkey binding id that triggers a turn |
| `DAEMON_PLUGIN_PTT_MAX_HOLD_MS` | `60000` | ptt mode: stuck-key auto-release cap (500–600000) |
| `DAEMON_PLUGIN_GAP_MS` | `2000` | idle gap between loop turns (50–3600000) |
| `DAEMON_PLUGIN_SAMPLE_RATE_HZ` | `16000` | capture rate negotiated with stt |
| `DAEMON_PLUGIN_CHUNK_MS` | `100` | mic chunk duration |
| `DAEMON_PLUGIN_STREAM_ID` | `7` | stream id shared by stt_listen_* and mic_start |
| `DAEMON_PLUGIN_TTS_PROVIDER` | `sherpa` | `sherpa` \| `openai` \| `elevenlabs` |
| `DAEMON_PLUGIN_TTS_VOICE` | `af_heart` | provider-specific voice id |
| `DAEMON_PLUGIN_TTS_FORMAT` | `wav` | synthesis format handed to sound_play |
| `DAEMON_PLUGIN_MAX_STEPS` | `6` | agent goal budget per turn (1–16) |
| `DAEMON_PLUGIN_TIMEOUT_MS` | `30000` | timeout for mic/stt/tts/sound round-trips |
| `DAEMON_PLUGIN_GOAL_TIMEOUT_MS` | `120000` | timeout for `goal_start` — LLM loops run long |

Cloud TTS providers additionally need their key on tts's own allowlist
(`TTS_PLUGIN_ALLOWED_KEY_ENVS` + vault/env) — the daemon never touches keys.

## Drop-in example

`~/.config/vyn/plugins.d/daemon.yaml` (always-on lifecycle comes from the
kernel's drop-in auto-spawn):

```yaml
binary: ~/.local/lib/vyn/plugins/daemon/daemon
sandbox: true            # no sockets of its own; all I/O is kernel-routed
auto_start: true
env:
  - DAEMON_PLUGIN_ENABLED=true
  # Hands-free ("walk & talk"): end turns on silence instead of a timer.
  - DAEMON_PLUGIN_MODE=vad
  # Or push-to-talk with the hotkey plugin:
  # - DAEMON_PLUGIN_MODE=ptt
  # - DAEMON_PLUGIN_PTT_BINDING=ptt
  # Under a secured kernel also add a JWT whose sub=daemon and whose
  # claims carry PERMISSION_AUDIO + PERMISSION_EVENT_PUBLISH (claims
  # override the manifest). The frame-MAC key is injected by the kernel:
  # - VYN_JWT_TOKEN=…
```

## Error reference

| Error | When |
|---|---|
| `ERR_DAEMON_BAD_PARAMS: … <field>` | malformed request; names the field |
| `unknown action: <name>` | typo'd action name |
| `ERR_DAEMON_BUSY: another voice turn is already in progress` | concurrent `daemon_turn` |
| `<stage> timed out after N ms` | downstream plugin didn't answer in time |
| `<stage> failed: <detail>` | downstream plugin returned ACTION_ERROR |
| `<stage> returned no <field>` | downstream answered OK but the payload was unexpected |

Stage names: `stt_listen_start`, `mic_start`, `mic_stop`,
`stt_listen_stop`, `goal_start`, `tts_synthesize`, `sound_play`.
