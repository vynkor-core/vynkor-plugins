# Agent token and reliability backlog — 2026-10-27

Measured on the live kernel (`ai.db` usage rows, `goal_get` steps) after the
token savers, the native-tool cap and the watchdog shipped. A typical goal is
now 2 steps × ~6k prompt tokens (was 2 × ~22k).

Done in this round: unknown-tool circuit breaker (2 misses → turn without
tools, a 3rd → canned answer), no memory extraction from goals that hit an
unknown tool, `[group]` labels in the tool overview, a steps-per-goal alert,
the prompt rule that called overview tools invalid, the routine reply guard
mixing words of two phrases, and the metrics store (chunked reads; the trim
had been failing silently since ~14k samples and the store reached 46k).

## Open

### 1. Plugins that lose the kernel connection exit 0 and stay dead (bug)

`vynkor-sdk` 0.0.3 `run_concurrent_loop` breaks on any `recv()` error
(`Err(_) => break, // disconnect / EOF`) and returns `Ok(())`. The plugin
exits with code 0, and the supervisor treats that as a deliberate stop
(`success=true will_restart=false`). Live at 09:39 and 09:49 the database
plugin dropped its connection this way; every `db_*` call then failed with
"action not found" (metrics, routines, goals) until a manual
`vyn plugin start database`.

Trigger: the fixed metrics trim draining its 46k backlog at 2000
`db_delete` a tick. Each delete publishes a change event; deletes timed
out at 5 s and the connection went down. With 200 a tick it held. Which
`recv()` error it was is not logged (the SDK drops it); the kernel saw a
plain EOF, no oversized/timeout frame. A burst of writes from any caller
can still do this.

Fix belongs in the SDK (return an error on an unexpected disconnect, `Ok`
only on `PluginShutdown`) or in the supervisor (restart a clean exit that
was not asked for). Either way it is an SDK release or a kernel change, not
a plugin patch.

Mitigation applied live: `plugins.d/database.yaml` uses `restart: always`
(`max_restarts: 20`). An explicit `vyn plugin stop` still stops it (the
supervisor records the stopped epoch and ignores that exit). A third
silent exit at 11:06 came after the metrics backlog had drained, so the
trim was not the only trigger. Other plugins on `serve_concurrent` still
use `on-failure`.

### 1b. "Open an app" (launcher) — findings 2026-10-27

Fixed: the agent JWT lacked PERMISSION_LAUNCH/CLIPBOARD/AUDIO (re-minted);
tools of disabled plugins (telegram, scheduler, ping-pong: 23) are left out
of the catalog, so "открой телеграм" no longer goes to `tg_*`; small tool
families come whole (`launch` with `launch_list`); a call missing required
params errors back to the model instead of asking the operator to approve
it; the unfiltered `launch_list` is compact (it was 14k chars, cut at 8k,
and read as "Telegram is not installed"); the truncation marker says the
rest is hidden.

Then split (operator's choice): `launch` opens desktop apps and Steam games
only, medium risk, no confirmation; terminal providers and `args` go to
`launch_command` (high, confirmed). `prompts.d/audio-voice.md` no longer
lets a `daemon_say` stand in for the action. Live: "открой телеграм" and
"запусти дискорд" both launch, in 3-6 steps.

Open:
- The model's first guess is often `launch` with no `app_id` (or
  `launch_command {"command": ...}`); it recovers via launch_list, but
  that is 2-3 wasted steps. A learned routine per app would skip them.
- Memory stores duplicates ("Бехзод пользуется Телеграмом" ×4).

### 2. The second step costs as much as the first

Step 2 usually only phrases the tool result, but re-sends the full system
prompt and tool schemas (~6k). The transcript is append-only and the tools
are fixed per goal, so the prefix is cache-friendly: local ollama reuses it
on its own (`KEEP_ALIVE=-1`, constant `num_ctx`). Open question: does
`gemma4:31b-cloud` cache prefixes or bill them? Measure before changing
anything. Ideas if it does not: send step 2 without the tool schemas when
the tool result is small and the call was a read (risk: the model needs a
second tool and cannot call it).

Reference: "Don't Break the Cache" (arXiv 2601.06007) — 45–80% cost cut
from prompt caching when dynamic content stays at the end.

### 3. Model cascade (local first, cloud on failure)

FrugalGPT-style: simple goals to qwen2.5:7b, escalate to the cloud model on
an error or an unusable answer. Costs: 5–15 s a step on CPU and weaker tool
calling. Needs a bake-off on real goals first; not worth it while the cloud
model is cheap.

### 4. Semantic answer cache: rejected

Read answers (RAM, battery, CPU) change every time; actions are covered by
learned routines. Known failure modes (follow-ups like "а теперь выключи",
negation "не включай" embedding next to "включи") are what the routine guard
already rejects.

### 6. `daemon_ask` inside goals

"Сколько оперативной памяти занято?" went through `daemon_ask` (a nested
goal, ~43k tokens for one question). The daemon plugin has uncommitted owner
changes; look at it together with them.

### 7. Smaller items

- Bake-off of qwen3:8b / gemma2:9b for routine mining (deferred).
- Routine trigger "включи диот светозерк" (STT garbage) stays as a phrase:
  harmless, STT may mishear the same way again.
- `metrics_query` still loads every sample (chunked) to filter by time; ids
  are in write order, so it could walk back from the newest instead.
