# agent plugin roadmap

The multi-step goal loop (`ai` chat + tool-call dispatch to other plugins'
actions, state persisted in `database`). Shipped as v0.1.0 — see README.md.

## v0.1 (shipped)

- Synchronous `goal_start` loop: model reply → tool dispatch → observation,
  bounded by `max_steps`, persisted per step (`goal:<id>` docs, atomic id
  counter).
- **Runtime manifest discovery**: tool specs (description, params schema,
  risk, `requires_confirmation`) are pulled from registered plugins'
  manifests via the kernel's read-only `list_plugins`/`get_manifest`
  commands — no admin permission needed (`READONLY_COMMANDS` exemption
  landed in the kernel for exactly this). Precedence per name:
  operator tools file > kernel manifest > minimal spec.
- Operator-gated dispatch: `AGENT_PLUGIN_ALLOWED_ACTIONS` allowlist is the
  security boundary independent of discovery.
- Prompt-side tool-calling protocol (no native tool-use blocks — `ai`'s
  normalized interface is plain text): final answer OR one
  `{"tool", "params"}` JSON object; forgiving parser (fences, embedded
  objects), malformed calls degrade to the final answer.
- Confirmation gate: confirmation-marked tools halt in
  `needs_confirmation`; `goal_resume {approve}` dispatches-or-declines.
  Engine never self-confirms.
- `goal_get`/`goal_list`/`tools_list` (with per-tool `source`);
  best-effort `plugin.agent.changed` events; transcript + observation size
  caps; LLM failures land in goal status, not the action error channel.

## Planned

- **Native tool-use passthrough** — **Phase 1 shipped in 0.1.4**: the
  allowlisted catalog rides as `ai`'s native `tools` param and structured
  `tool_calls` replies dispatch without text heuristics; the prompt-side
  text protocol stays as the permanent fallback (models without tool
  support, providers rejecting the param → per-goal degrade, remembered in
  the goal doc). Remaining Phase 2 (unscheduled): true multi-turn history —
  replaying assistant tool_use / tool_result blocks via an ai Message-blocks
  extension — only if models prove confused by the flat-text transcript.
- **Background goals** — detach long goals from the caller: accept → run on
  an internal task (calendar-style select branch) with progress events and
  `goal_status` polling. Blocked on a real consumer needing >30 s goals
  (the sync path already streams nothing).
- **Memory** — vector-db-backed long-term memory (facts between goals) —
  **shipped (v0.1.0, 2026-08-26)**.
- **Streaming steps** — per-step events for webclient UIs.

## Non-goals

- No shell/exec plugin usage — narrow-permission-per-plugin holds; the
  agent only dispatches catalogued actions under their own permissions.
- No self-modification of its own allowlist/catalog at runtime — operator
  surfaces only (env + tools file).
- No multi-goal concurrency within one plugin instance beyond what the
  serve loop naturally allows; goal documents are independent, ordering is
  first-come.
