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
(`success=true will_restart=false`). Live at 09:39:01 the database plugin
dropped its connection this way; every `db_*` call then failed with
"action not found" (metrics, routines, goals) until a manual
`vyn plugin start database`. Why the connection closed is unknown (no
warning in the kernel journal).

Fix belongs in the SDK (return an error on an unexpected disconnect, `Ok`
only on `PluginShutdown`) or in the supervisor (restart a clean exit that
was not asked for). Either way it is an SDK release or a kernel change, not
a plugin patch.

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
