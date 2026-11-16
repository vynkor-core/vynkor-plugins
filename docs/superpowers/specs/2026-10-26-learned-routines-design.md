# Learned routines — design

Date: 2026-10-26. Plugin: `agent`.

## Intent

Most of what the operator asks the agent is the same handful of device
commands ("включи свет", "выключи светодиод", "фонарик"). Each one costs a
full cloud LLM round-trip (two calls, ~3k–22k input tokens) to arrive at the
same single tool call it arrived at last time. The agent should notice
these on its own, overnight, with a local model, and from then on answer
them directly: dispatch the known tool call, reply with a pre-generated
in-character phrase, no LLM at all.

Operator decisions (2026-10-26):

- target is **device commands** first;
- the agent **discovers routines itself**, in the background at night, on
  a **local** model;
- discovered routines **activate automatically** (no per-routine approval);
- replies are **pre-generated** at discovery time, **several variants**,
  so the instant answer does not sound canned.

## Goals

- A repeated device command is served with zero LLM calls and sub-second
  latency.
- Discovery spends zero cloud tokens.
- A routine that turns out wrong stops firing by itself, without the
  operator editing anything.

## Non-goals (v1)

- **Slots / parameters.** "свет на кухне" and "свет в спальне" are two
  routines, not one routine with a `room` slot. Revisit once there is more
  than one controllable light.
- **Read tools.** "какой заряд батареи" needs the tool *result* in the
  reply; v1 only learns action tools whose reply does not depend on the
  result.
- **Multi-step goals.** Only goals that completed with exactly one
  successful tool call are mined.
- Any UI. Operator inspection is via actions (`routine_list`,
  `routine_delete`).

## Anti-goals (failure even if it "works")

- A routine fires on a request that meant something else, and nothing
  notices.
- Discovery quietly burns cloud tokens (e.g. falls back to the default
  cloud profile when the local model is down).
- A routine keeps firing after its tool changed or was removed.
- A routine ever dispatches a confirmation-gated or high-risk tool.

## Constraints

- Lives inside the `agent` plugin (the `scheduler` plugin is disabled on
  the live host); the nightly job is an in-process tokio timer.
- Persistence through the existing `database` RPC (same namespace as goal
  docs); no new plugin, no new kernel permission.
- Local model through `ai` by agent profile id, like
  `AGENT_PLUGIN_MEMORY_AGENT_ID` (live: `memory` → `llama3.2:3b`).
- Goal docs already record what is needed: `goal`, `status`, and
  `steps[].detail = {tool, params}` for `tool_ok` / `tool_error`.

## Design

### 1. Eligibility

A tool may be learned only if all hold:

- it is on `AGENT_PLUGIN_ROUTINE_TOOLS` (comma list, exact names). Default
  when unset: `mqtt_device_command,pixel-6pro.flashlight,pixel-6pro.dnd,
  pixel-6pro.ringer,pixel-6pro.brightness`. An allowlist, not a denylist:
  the live catalog has `tg_send_message`, `pixel-6pro.sms` etc. with no
  confirmation flag, and those must never be learned.
- the catalog spec has `requires_confirmation == false` and `risk` not
  `high` (checked both at mining time and at fire time).

### 2. Mining (nightly)

Timer: `AGENT_PLUGIN_ROUTINES_AT=HH:MM` (local time, default `03:30`);
unset `AGENT_PLUGIN_ROUTINES` / not `on` disables the whole feature. Also
an action `routine_mine` to run it on demand (testing, first run).

1. Load completed goals from the last `AGENT_PLUGIN_ROUTINES_LOOKBACK_DAYS`
   (default 30).
2. Keep goals with exactly one `tool_ok` step, zero `tool_error` steps, an
   eligible tool, and a goal text that passes the phrase check in step 6.
3. Normalize the goal text with the same normalizer as small talk
   (lowercase, `ё→е`, non-alphanumerics → space, collapse whitespace).
4. Group by `(tool, canonical JSON of params)`. A group is a candidate when
   it has **≥ 2 successes**. If the same normalized text appears in two
   different groups (same words, different calls), that text is
   **conflicting** and is dropped everywhere.
5. Per candidate, one local-model call (`purpose: routine_mine`) with the
   observed goal texts, tool name, description, params and, for a switch
   call, its direction ("switches the device OFF"). It must return JSON:
   - `paraphrases: [string]` — up to 8 other ways to say exactly this
     command, in the operator's language.
   - `replies: [string]` — 8–10 short in-character confirmations, persona
     from `persona_block()` (channel note: must read well in text **and**
     voice).
6. Validate deterministically (`routines/guard.rs`) — nothing the model
   writes is trusted unchecked. Live runs on qwen2.5:3b produced "зажигай"
   as a paraphrase of an off call, "Такой команды нет" as a success reply,
   Latin look-alikes ("Светодiод"), invented devices ("Подсвечник") and a
   random context flag, so the model's own context judgement was removed:
   - calls that **read** state (`command: get_state`, `phone.get_battery`)
     are never learned — no reply written in advance can be right;
   - a phrase (observed or paraphrase) needs ≥ 2 words, no back-reference
     or meaning-bending word (его, это, тоже, ещё, кроме, не, то же самое…),
     no mixed-script word, and for a switch call (`value`/`state`/`on`…
     = 0/1/true/false/on/off) it must state the call's own direction;
   - a paraphrase must name exactly the objects (by stem) of one observed
     phrase: verb and word order may change, the device may not
     ("выключи светодиод" never grows "выключи свет");
   - a reply must not report a failure, must state a switch call's new
     state, and past two words must name an observed object;
   - fewer than 3 surviving replies are topped up from a fixed stock set
     per direction ("Выключил.", "Готово, выключено.", "Выключено.");
   - paraphrases owned by a different routine, or conflicting texts, are
     dropped; observed goal texts that pass the phrase check always stay.
7. Upsert the routine (merging with an existing one for the same
   `(tool, params)`: union phrases, replace replies).

If the local model call fails, that candidate is skipped this night —
**never** retried on the goal's cloud profile (anti-goal). The fallback
agent applies only if it is itself local; v1 simply passes no fallback.

### 3. Storage

`database` docs:

- `routine:<id>` — `{id, tool, params, phrases: [normalized], replies:
  [string], spec_hash, status: active|disabled, disabled_reason, hits,
  fails, last_reply_idx, created_at_ms, updated_at_ms}`
- `routines:index` — list of ids (same pattern as `memory:index`).

`spec_hash` = hash of the tool's name + description + parameters schema at
mining time.

### 4. Firing

At `goal_start`, `Entry::Fresh`, before small talk and before any
embedding/LLM work:

1. Normalize the goal; look it up in an in-memory phrase → routine map
   (loaded on start, refreshed after mining and after any routine write).
2. Hit on an `active` routine → re-check eligibility against the *current*
   catalog and compare `spec_hash`. Mismatch → disable
   (`disabled_reason: tool_changed`) and fall through to the normal path.
3. Dispatch through the same `dispatch_and_observe` (quota, device retry,
   observation turn and step record all as today). Step detail gains
   `"routine": <id>`.
4. `tool_ok` → final answer = a reply variant, rotating
   (`last_reply_idx + 1`), never the same one twice in a row; `hits += 1`;
   goal completed. Memory extraction is skipped.
5. `tool_error` → `fails += 1`, routine disabled
   (`disabled_reason: tool_error`), and the goal continues on the normal
   LLM loop with the error observation already in the transcript.

### 5. Self-correction

If the next goal within 120 s of a routine hit (any caller — goal docs
carry no session id, and the host has a single operator)
normalizes to start with a correction marker (`не то`, `не тот`, `не так`,
`неправильно`, `отмена`, `отмени`, `верни`), the routine that just fired is
disabled (`disabled_reason: user_correction`) and that goal runs normally.
A disabled routine is not re-learned from goals older than the disable
time.

### 6. Operator surface

- `routine_list` → all routines with status, phrases, hits/fails.
- `routine_delete {id}` → removes it (and its index entry).
- `routine_mine` → run mining now, returns `{candidates, created, updated,
  dropped}`.
- Usage rows: mining calls are tagged `purpose: routine_mine`; fired
  routines produce no usage row, and the goal step carries `routine`.

## Settled by the builder (my calls, flagged for review)

- Allowlist default above (MQTT command + phone toggles).
- ≥ 2 successes to learn; one tool error or one correction disables.
- Correction marker list and the 120 s window.
- Firing order: routines before small talk.
- Discovery time 03:30 local.

## Left to the builder

- Exact prompt wording for the mining call.
- How the in-memory phrase map is refreshed (reload-after-write is
  enough).
- Test strategy (the existing `main.rs` shim already scripts
  `chat_completion` and records requests).
