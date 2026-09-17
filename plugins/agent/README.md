# agent plugin

Multi-step goal loop for vynkor plugins: takes a natural-language goal,
plans and acts by calling other plugins' kernel-routed actions as tools
(through `ai`'s `chat_completion`), and persists every goal as a JSON
document in its own `database` namespace.

This is the integration point the root `ROADMAP.md` reserves: no business
primitives of its own — LLM traffic routes through `ai`, state through
`database`, tools through the kernel to whichever plugin owns the action.

Tool calling is dual-path. **Native passthrough** (default): the catalog is
sent as `ai`'s `tools` param and the model's invocations come back as
structured `tool_calls` — parsed without text heuristics. The text protocol
below stays as a permanent fallback: models that ignore `tools`, providers
without tool support, or a provider rejecting the param mid-goal (the goal
degrades to text-only and remembers it) all keep working through the same
forgiving prompt-side parser (`{"tool": ..., "params": {...}}`, fenced
blocks unwrapped, qwen-style `<tool_call>` drift tolerated).

## Operator note

`agent` declares two kernel permissions — `PERMISSION_STORAGE` +
`PERMISSION_EVENT_PUBLISH` (same shape as `scheduler`). It holds
`PERMISSION_STORAGE` because it calls `database`'s gated actions directly
(T-19 anti-laundering).

**Permissions for dispatched tools are NOT declared in this manifest** —
they come from the operator's JWT grant for `agent`. T-19 requires the
*caller* of a gated action to hold that action's permission, so if you put
`notify_send` on the allowlist, mint the agent's token with
`PERMISSION_NOTIFY` too; a gated call without it fails into the loop as an
ordinary tool error (`last_error` style) — never laundered. Undeclared
ungated actions (`media`, most of `system`) need nothing extra.

Requires `network` + `ai` registered (LLM leg); `database` registered
(state). The key named by `AGENT_PLUGIN_AI_API_KEY_ENV` must be on `ai`'s
own `AI_PLUGIN_ALLOWED_KEY_ENVS` allowlist and resolves vault-first via
`secrets`.

## Actions

### `goal_start`

```json
{ "goal": "Find the loudest tab and duck it to 30%", "max_steps": 6 }
```

Runs the loop synchronously and returns when the goal reaches a terminal
or halting status:

```json
{
  "id": "7", "status": "completed", "title": "…",
  "final_answer": "Ducked Chrome to 30%.",
  "error": "", "steps": [{"n": 1, "kind": "tool_ok", "detail": {"tool": "sys_volume_set"}}],
  "pending_tool": "", "max_steps": 6
}
```

Statuses: `completed` | `needs_confirmation` | `declined` |
`max_steps_reached` | `error`. Optional overrides per goal: `context`,
`title`, `max_steps` (1..=16), `provider`/`base_url`/`model`/
`api_key_env`/`agent_id`/`max_tokens` (LLM routing; explicit fields beat
env defaults, `agent_id` switches `ai` into profile mode).

Errors: malformed params (`ERR_AGENT_BAD_PARAMS` naming the field),
storage failures, `ai` transport errors (land as status `error`, not
`ACTION_ERROR`, once the goal exists).

### `goal_get` / `goal_list`

```json
{"id": "7"}        // → {"found": true, "goal": {full doc incl. transcript}}
{}                 // list: {"total": 2, "goals": [newest first]}
```

Missing reads are `{found: false}`; corrupt stored docs fail loudly.

### `goal_resume`

A goal halts in `needs_confirmation` when the model proposes a tool whose
spec marks `requires_confirmation: true`. The engine never self-confirms:

```json
{"id": "7", "approve": true}
```

`approve: true` dispatches exactly the pending call (with the exact pending
params) and continues the loop; `approve: false` declines the goal
(`status: declined`, final answer names the declined tool). Resuming a goal
that isn't awaiting confirmation is an error.

### `tools_list`

Returns the effective catalog the model sees:
`{tools: [{name, description, parameters, requires_confirmation,
timeout_ms}], allowed_actions, tools_file_set}`.

### `memory_forget` / `memory_clear` / `memory_list` (AGT-01, opt-in)

With `AGENT_PLUGIN_MEMORY=on`, completed goals get an extraction pass (one
extra cheap `chat_completion`) and durable facts land in the `vector-db`
plugin (embedding routed through `ai`), collection
`AGENT_PLUGIN_MEMORY_COLLECTION` (default `agent-memory`). Fresh goals
recall up-to-5 similar facts scoring ≥0.72 into the transcript as a leading
`[KNOWN CONTEXT]` turn.

```json
{"query": "which server does the user run"}  // → {"forgotten": true, "id": "f1787…-0"}
{}                                           // clear → {"cleared": 12}; list → {"total": n, "facts": [...]}
```

`forget` deletes the top semantic match above the score floor; `clear`
wipes every indexed fact deterministically (vector-db has no collection
wipe, so ids live in our own `database` key `memory:index`). Extraction/
recall failures log loudly and never fail a goal. Memory is **off** by
default — turning it on is a privacy decision; all state stays in this
plugin's per-caller namespaces.

## Tool catalog (operator-gated, manifest-discovered)

The catalog is built from three layers, merged per allowlisted action name
with this precedence:

1. **Operator tools file** (`AGENT_PLUGIN_TOOLS_FILE`, optional) — wins:
   hand-written descriptions/schemas are deliberate.
2. **Kernel manifests** — on every goal start the agent calls the kernel's
   read-only `list_plugins` + `get_manifest` commands and fills any tool
   that has no file entry from the owning plugin's registered manifest:
   description, `params_schema` (decoded into a JSON object for the
   prompt), `risk`, and `requires_confirmation`. This is the default path;
   no file is needed. These commands are exempt from
   `PERMISSION_KERNEL_ADMIN` (read-only, public distribution data) — see
   the kernel's `READONLY_COMMANDS` — so the agent holds no admin.
3. **Minimal spec** — an allowlisted name with neither source still
   dispatches, with empty description and no schema.

`AGENT_PLUGIN_ALLOWED_ACTIONS` stays the security boundary regardless of
source: comma-separated exact action names, default-deny, and nothing
outside it is ever dispatched whatever the manifests say. `tools_list`
reports each tool's `source` (`file` / `kernel` / `minimal`) so operators
can see exactly what the model sees. On older kernels without the commands
the agent logs loudly and degrades to layers 1+3; `AGENT_PLUGIN_DISCOVERY=off`
skips the round-trips entirely.

## Configuration

Env vars set in the kernel's config under this plugin's `env:` list — see
`config.example.yaml`.

| Variable | Default | Meaning |
|---|---|---|
| `AGENT_PLUGIN_ALLOWED_ACTIONS` | *(unset = deny-all)* | Comma-separated action names the model may dispatch. |
| `AGENT_PLUGIN_TOOLS_FILE` | *(unset)* | Path to the tool description JSON. |
| `AGENT_PLUGIN_NATIVE_TOOLS` | `auto` | Native tool-use passthrough: `auto` sends `ai` a `tools` param when the catalog is non-empty; `on` forces it; `off` is text-protocol only. |
| `AGENT_PLUGIN_AI_MAX_RETRIES` | `0` | Provider 429/5xx retries delegated to `ai`. Keep 0 for interactive goals — ai's serve loop is sequential, and long retry chains trip the kernel watchdog; fail-fast + `AGENT_PLUGIN_FALLBACK_AGENT_ID` covers transient errors instead. |
| `AGENT_PLUGIN_AI_TIMEOUT_MS` | `30000` | Per-completion timeout (clamped 1s..300s). Raise it for local CPU models — and raise the kernel watchdog (`watchdog_interval_secs` + `watchdog_timeout_secs`) to match, since ai stays blocked for the duration. |
| `AGENT_PLUGIN_AI_PROVIDER` | `openai` | `anthropic` \| `openai` for `ai.chat_completion`. |
| `AGENT_PLUGIN_AI_BASE_URL` | *(ai default)* | OpenAI-compatible base URL override. |
| `AGENT_PLUGIN_AI_MODEL` | *(none)* | Default model id. |
| `AGENT_PLUGIN_AI_API_KEY_ENV` | *(none)* | Env-var-style key handle (never a literal key). |
| `AGENT_PLUGIN_AI_AGENT_ID` | *(none)* | Named `ai` profile; overrides the explicit defaults above. |
| `AGENT_PLUGIN_AI_MAX_TOKENS` | `1024` | Per-completion token cap. |
| `AGENT_PLUGIN_MAX_STEPS` | `6` | Default loop budget (1..=16). |
| `AGENT_PLUGIN_DB_TIMEOUT_MS` | `5000` | Per-call timeout for `database` round-trips. |
| `AGENT_PLUGIN_PROMPTS_DIR` | *(unset)* | Directory of prompt files. Group prompts at `<dir>/<group>.md` (`/` in a group name becomes `-`, so `audio/voice` → `audio-voice.md`). Reserved stems: `_identity.md`, `_personality.md`, `_channel.md` override the built-in persona constants; `_facts.md` supplies the facts block. A missing or blank file falls through to the next layer, never to an empty prompt. |
| `AGENT_PLUGIN_FACTS` | *(unset)* | Literal facts text; overrides `_facts.md`. |

Group-prompt precedence: `AGENT_PLUGIN_PLUGIN_PROMPTS` (wins) >
`<prompts dir>/<group>.md` > built-in default. Each layer replaces the
next; there is no append.

## Security

- Allowlist-gated dispatch, exact-match, default-deny.
- Confirmation-marked tools halt the goal until an approved resume; the
  engine never confirms its own calls (D-09 spirit).
- Every mutation publishes best-effort `plugin.agent.changed`
  `{op, id}` after the response (never blocks/fails the caller).
- Transcript and single tool results are size-capped (256 KB / 8 KB chars)
  so one chatty plugin can't blow the context or the storage doc.
- `max_steps` bounds the loop; runaway goals end in
  `max_steps_reached`, not infinite spend.
- No exec surface of its own: everything routes through kernel-routed
  actions under their own permissions.

## Testing

`cargo test -p agent-plugin --manifest-path plugins/agent/Cargo.toml` —
35 unit + 13 fake-kernel e2e tests over `UnixStream::pair` (scripted
`chat_completion` replies — text-protocol and native `tool_calls` shapes —,
fake `database`, dispatch recorder): happy path, native dispatch + tools
param forwarding, unknown-tool feedback, confirmation halt +
approve/decline, max-steps budget, LLM failure, persistence/listing,
validation errors.
