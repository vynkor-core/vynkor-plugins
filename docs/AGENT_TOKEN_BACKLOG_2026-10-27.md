# Agent token and reliability backlog — 2026-10-27

Measured on the live kernel (`ai.db` usage rows, `goal_get` steps) after the
token savers, the native-tool cap and the watchdog shipped. A typical goal is
now 2 steps × ~6k prompt tokens (was 2 × ~22k).

Done in this round: unknown-tool circuit breaker (2 misses → turn without
tools, a 3rd → canned answer), no memory extraction from goals that hit an
unknown tool, `[group]` labels in the tool overview.

## Open

### 1. metrics plugin: every action fails past ~4 MB of samples (bug)

`metrics_query` / `metrics_latest` / `metrics_stats` all call `db.list()`,
which loads every sample through one `db_batch_get`. With the default
`METRICS_PLUGIN_MAX_SAMPLES=10000` the batch passes the database plugin's
`max_response_bytes` (4 MiB):

    metrics_latest failed: db_batch_get failed: batch_get result exceeds max_response_bytes (> 4194304)

Seen live on "Какая температура видеокарты?". Fix options: fetch keys in
chunks; keep the newest sample under its own key for `metrics_latest`; or at
least a default `max_samples` that fits (≈ 5000). Workaround now: set
`METRICS_PLUGIN_MAX_SAMPLES` lower in `plugins.d/metrics.yaml` and restart.

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

### 5. Watchdog: watch goals, not only prompt size

The watchdog averages prompt tokens per step. The guessing loop this round
fixed had normal-sized steps (~4.5k) but 7 of them, so it never fired. Add
an alert on steps per goal (e.g. > 4 for several goals) or on the share of
`unknown_tool` steps.

### 6. `daemon_ask` inside goals

"Сколько оперативной памяти занято?" went through `daemon_ask` (a nested
goal, ~43k tokens for one question). The daemon plugin has uncommitted owner
changes; look at it together with them.

### 7. Smaller items

- Bake-off of qwen3:8b / gemma2:9b for routine mining (deferred).
- Routine phrase "включи диот светозерк" (STT garbage) is still stored.
- The reply rule "Only tool names from the detailed list above are valid"
  contradicts the overview being callable; harmless now, revisit if models
  start refusing overview tools.
