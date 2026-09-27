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

## Known issues

- **`status` action name collision — resolved (STAT-01).** Nine plugins
  declared a bare `status`; the kernel refuses to route an action declared
  by more than one plugin (`ActionLookup::Ambiguous` → `ACTION_NOT_FOUND`),
  so none was reachable. Each now declares `<slug>_status`
  (`network_status`, `metrics_status`, …); allowlist those names.
- **`AGENT_PLUGIN_ALLOWED_ACTIONS` is baked in at process start, not
  hot-reloaded.** The catalog rebuild described above (tools file +
  discovery, "per goal start") only rereads the tools *file*; the allowlist
  itself is `std::env::var` read once in `Catalog::load()`. Adding actions
  to a plugin and even restarting the kernel so the new plugin *process*
  starts is not sufficient — the `agent` plugin's own process must restart
  after `plugins.d/agent.yaml`'s `AGENT_PLUGIN_ALLOWED_ACTIONS` is edited,
  or the new actions stay invisible to `tools_list` even though the
  target plugin is live and answering IPC. In practice this means two
  `vyn restart` cycles when deploying a plugin that wasn't previously
  allowlisted: one for the new plugin binary + its `plugins.d/<id>.yaml`
  drop-in to come up, one after editing the allowlist.

## Non-goals

- No shell/exec plugin usage — narrow-permission-per-plugin holds; the
  agent only dispatches catalogued actions under their own permissions.
- No self-modification of its own allowlist/catalog at runtime — operator
  surfaces only (env + tools file).
- No multi-goal concurrency within one plugin instance beyond what the
  serve loop naturally allows; goal documents are independent, ordering is
  first-come.

## Audit 2026-09-27 — next

- **`goal_cancel {id}`** (AGT-10) — stop a running goal between steps; no
  way to abort a runaway goal exists today short of restarting the plugin.
- **Step events** — `plugin.agent.step {goal_id, n, tool, ok}` so
  webclient/daemon/telegram can show progress (see "Streaming steps").
- **Token/cost budgets** per goal and per day (AGT-06).
