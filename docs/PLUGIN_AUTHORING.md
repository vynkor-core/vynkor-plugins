# Plugin authoring notes

Practical lessons from building `notes` and `calendar` (2026-08), written so
the next plugin doesn't rediscover them the hard way. Every pattern here is
implemented and tested in `plugins/notes/src/` and `plugins/calendar/src/`;
the original custom-loop variant is `plugins/sync-client/src/lib.rs`.

## 1. The single-reader rule — `send_action` discards frames

`VynkorClient::send_action` loops on `recv_timeout` while awaiting its
response and **discards every inbound frame that does not match**
(see its doc: "Frames that arrive while waiting ... are discarded").
Consequences:

- Only a task that owns 100% of the connection's traffic may call it.
- Anything else — a spawned handler task, a timer-driven scan — must route
  outbound calls through a proxy owned by the loop task. Direct use will
  silently eat concurrent traffic: user requests arriving mid-scan, kernel
  pings during a slow database round-trip.

This bit for real: calendar's first version ran the reminder scan inline on
a timer tick; every test call landing inside the scan's first `db_keys`
round-trip vanished.

**The pattern** (single-reader loop + channel-fronted RPC proxy):

- The serve loop exclusively owns `VynkorClient`; `tokio::select!` over
  `client.recv()`, an outbound `mpsc<Envelope>` channel, an RPC request
  channel, and (calendar) the scan interval tick.
- Handler/scan tasks get a cloneable `Rpc { tx }` handle; each call sends
  `RpcCall { action, params_json, timeout_ms, reply: oneshot }`.
- The loop assigns `rpc-{seq}` action ids, keeps a pending map
  `id -> (action, reply)`, and completes entries when the matching
  `ActionResponse` arrives (decode `data_json` on `ACTION_OK`, else surface
  `error`). Nothing inbound is ever dropped.
- Replies and fire-and-forget event envelopes flow back through the outbound
  channel; FIFO preserves the response-before-event ordering contract.

Hot-path storage plugins making **no** outbound calls don't need this — they
implement the SDK's `ConcurrentHandler` instead (`database`, `network`).
Background-task plugins with fire-and-forget writes can push raw envelopes
into the loop's channel like sync-client's heartbeat. Choose by shape:

| Plugin shape | Loop |
|---|---|
| hot path, no outbound IPC | SDK `serve_concurrent` / `ConcurrentHandler` |
| thin wrapper calling other plugins | sequential loop + RPC proxy (notes) |
| timer/background activity + outbound RPC | single-reader select loop + RPC proxy + spawned tasks (calendar) |

## 2. Kernel routing facts — what a caller must declare

- Actions route by manifest declaration: the kernel resolves the provider
  via `find_action_provider` (`vynkor/src/ipc/protocol.rs`) and refuses
  ambiguous declarations. Declare every action you serve.
- Manifest v2 per-action `permission` is enforced on **both provider and
  caller** (data-driven T-19 anti-laundering). A wrapper plugin that calls
  gated actions must itself hold those permissions: `notes` holds
  `PERMISSION_STORAGE`; `calendar` holds `STORAGE` + `NOTIFY`. Callers of
  the wrapper's own ungated actions need nothing.
- `PERMISSION_IPC_SEND` + `ipc_targets` gate **raw frame forwarding**
  (T-04) only — ordinary kernel-routed action calls need neither.
  Precedent: `notify` calls `tts_synthesize` with neither declared.
- `ActionRequest.caller_plugin_id` is stamped by the kernel from the
  authenticated sender (inbound value discarded). `database` namespaces
  storage per caller, so a wrapper plugin gets a private namespace free.

## 3. Testing against a fake kernel

`UnixStream::pair()` + `VynkorClient::from_stream` on both ends drives the
real serve loop without a live kernel (SDK test pattern, used by
`sync-client`, `notes`, `calendar`):

- **Handshake first.** The shim must answer `PluginRegister` before
  processing any test command: `register_full` treats the very next inbound
  frame as the ack, so a test command racing ahead kills the plugin with
  "expected PluginRegisterAck". Buffer commands in an mpsc channel and start
  draining only after acking registration.
- An in-memory `FakeDb` (BTreeMap KV) answering
  `db_incr/db_set/db_get/db_keys/db_batch_get/db_delete` exercises the real
  wire shapes end to end.
- Recorders make background activity assertable: collect `EventPublish`
  frames (respond `EventPublishAck` = `EVENT_PUBLISH_OK`) and
  `notify_send` requests.
- Assert events with polling helpers, not immediately: the plugin sends the
  `ActionResponse` BEFORE the event envelope, so a just-finished call races
  the recording. Timer-driven behavior (reminder scans) needs generous poll
  windows around the configured scan interval.

## 4. Thin-wrapper checklist

- Key layout `<entity>:<id>` JSON documents + `meta:next_id` id counter
  via `db_incr` (atomic, monotonic, survives restarts).
- Response first, then best-effort change event — a publish never delays or
  fails the caller's reply (database's contract).
- Validate loudly at parse time with shape-naming errors; serde enforces
  nothing beyond types (a manifest `"minimum": 0` is documentation, not a
  check).
- Missing-entity reads are `{found: false}` results; deletes are idempotent
  (`{deleted: false}`); updates of missing entities ARE errors.
- Manifest v2: object-form actions with input/output schemas,
  `config_schema`, env vars named `<PLUGIN>_PLUGIN_*`.
- Per-plugin docs: `README.md` (contract) + `ROADMAP.md` (non-goals) — see
  any shipped plugin for the pattern.

## 5. Sandbox path resolution (filesystem)

The filesystem plugin's jail (`plugins/filesystem/src/sandbox.rs`) is the
reference for any plugin that must confine I/O to operator-named roots.
The shape that survived review and tests:

- **Default-deny roots**: comma-separated absolute dirs from an env var;
  unset/empty rejects every action with an error that names the variable.
  Relative/nonexistent roots are logged loudly at startup and skipped.
- **Resolve = canonicalize deepest *existing* ancestor, then textually
  re-append the non-existing remainder**, then require the result to be
  component-wise (`Path::starts_with`, never string prefix) inside a
  canonical root. Canonicalizing the existing portion resolves every symlink
  component, so file symlinks pointing outside a root and symlinked dir
  components are rejected by the containment check itself — no separate
  symlink walk needed.
- **Reject `..` that survives into the remainder** (the textual join can't
  resolve it). `..` inside the existing portion is harmless: canonicalize
  already folded it. Gotcha that cost a test failure: `Path::file_name()`
  returns `None` for paths terminating in `..` — detect that case via
  `components().next_back() == Some(Component::ParentDir)`, not via
  `file_name()`.
- **Writes need one extra check**: after resolution, `symlink_metadata` the
  final component and refuse symlinks (a dangling symlink canonicalizes as
  its parent, so containment alone would let a write follow it outside).
- Document TOCTOU (check-then-use symlink swap) as a known non-goal unless
  you're prepared to go openat2/`RESOLVE_BENEATH`.

## 6. Testing handlers that read process env

Allowlist-style config (`*_ALLOWED_KEY_ENVS`, sandbox roots) is read from
process env inside handlers, which makes naive integration tests racy:
`#[tokio::test]` runs tests in parallel threads sharing one environment.
The search plugin's fake-kernel test shows the cheap fix: set every env var
once through a `static OnceLock<()>` helper called at the top of each test,
choosing values that work for all tests simultaneously (one fixed allowlist
covering both providers' key names; a decoy env value to prove vault-wins;
an unlisted name for the rejection case). No locks, no cleanup, no races —
and the vault-vs-env precedence gets tested for free.

## 7. Status convention (INF-07)

Every plugin should expose a `status` action (no input, always ungated) for
`vyn status` / web health dashboards:

```json
// request: {"action": "status", "params_json": "{}"}
// response data_json:
{
  "version": "0.1.0",
  "uptime_ms": 12345,
  "engine_ready": true,
  "last_error": null,
  "counters": {"handled": 42}
}
```

- `version` — plugin's own `PLUGIN_VERSION`.
- `uptime_ms` — `now - start_monotonic`.
- `engine_ready` — false while lazy-loaded model/DB not yet ready.
- `last_error` — last transient error string or null.
- `counters` — free-form metrics (requests, cache hits, etc.).

Implementations keep `start_instant = Instant::now()` at `main` startup and
return `elapsed`. Plugins with lazy engines (speech, vector-db) set
`engine_ready` from an `AtomicBool` flipped after `OnceLock` init. The helper
`vynkor_sdk::status::status_response(version, start, ready, last_error, counters)` builds the JSON; no kernel change needed — `status` is just another
registered action, polled via ordinary `send_action` to each plugin.

## 8. Running against a live secured kernel

Lessons from the first full audit (`LIVE_KERNEL_AUDIT_2026-08-22.md`,
harness snapshot in `scripts/live-audit/`). The fake kernel (§3) proves
handler logic; these are the things that only bite on a real secured one.

- **The supervisor injects nothing auth-related.** A plugin under a
  kernel with `jwt_secret` needs both `VYN_JWT_SECRET` (frame-MAC key
  derivation) and `VYNKOR_JWT_TOKEN`, added by the operator to the
  drop-in's `env:` list. The token's `sub` must equal the registering
  `plugin_id`, and its claims **override** the manifest — mint per plugin
  with that plugin's declared permissions. Missing either → registration
  rejected → exit → silent restart loop until `max_restarts` runs out,
  with an empty log ring buffer and nothing at WARN kernel-side. When a
  plugin "won't start", check this before anything else; run the binary
  manually with the drop-in env plus `VYN_SOCKET_PATH` to see the real
  error on stderr.
- **Action requests go to frame target `kernel`.** The kernel only does
  pending-action bookkeeping (internal `kact-*` correlation, response
  proxying, T-19 caller checks) for envelopes targeted at `kernel`.
  Targeting a plugin slug directly takes the zero-parse forward path:
  your request arrives verbatim, the reply carries *your* original
  `action_id`, matches no pending entry, and is silently dropped.
- **Wire types are strict**: `params_json`/`data_json` are protobuf
  `bytes` (serialize the JSON), and responses may arrive
  zstd-compressed (payloads ≥ 64 KiB, `FLAG_COMPRESSED`) or MAC-tagged —
  handle both when writing a client by hand (see `scripts/live-audit/
  vynkor_ws.py`).
- **`ipc_targets` is exact-match** — no wildcard. List every slug you
  will call.
- **The HTTP API is TLS-only by default**; plain-HTTP probes fail with a
  confusing non-HTTP reply. Use `https://` (and `wss://` on the socket)
  against a dev cert.
- **Plugin env inheritance is a real debugging surface**: desktop-facing
  plugins (clipboard/notify/media/system) depend on
  `WAYLAND_DISPLAY`/`DBUS_SESSION_BUS_ADDRESS` being present in the
  *kernel's* environment at spawn time — start the kernel from a session
  shell, not a stripped service context, or provider detection degrades
  in ways that look like plugin bugs.

## Action documentation and the agent catalog

The `agent` plugin builds the tool catalog the model sees by merging three
layers per action name (see `plugins/agent/README.md`): the operator's
tools file wins, the **kernel manifest** fills anything the file omits, and
a bare allowlisted name still dispatches with no schema.

The kernel-manifest layer is the one you own, and it is the one that scales.
It is fed by the `action_specs` field of the `PluginManifest` you send at
registration — **not** by your `plugin.json` alone. A plugin that leaves
`action_specs` at `Default::default()` contributes nothing to the catalog
and forces its tools to be hand-copied into an operator file that has no
link back to this repo. That is how a catalog comes to reference actions a
plugin never had.

### Required of every plugin

1. **Declare a `description` on every action in `plugin.json`.** One line,
   written for the model, not for a changelog: what the action does and
   when to reach for it. `scripts/check-action-docs.py` enforces this in
   CI. An empty description is worse than a bad one — a tool that embeds as
   `"name — "` is dropped outright by the agent's embedding filter
   (`AGENT_PLUGIN_EMBEDDING_FILTER`).
2. **Declare an `input` JSON Schema on every action that takes params**,
   with a `description` on each property. This becomes `params_schema`.
3. **Populate `action_specs` at registration.** Copy
   `plugins/telegram/src/manifest.rs` — it reads the installed `plugin.json`
   next to the binary and converts it, degrading to an empty list on any
   failure so registration never depends on the doc file. Add `plugin.json`
   to your manifest's `files` list so it is installed.
4. **Set `risk` and `requires_confirmation`** on anything destructive. For
   the two-step request/confirm pattern, use
   `vynkor_sdk::confirmation_gate::ConfirmationGate` and merge its
   `manifest_entries()` output instead of hand-writing the specs.

### Not required of you

Do not write entries into the operator's `agent-tools.json`. That file is an
operator override for when a plugin's own docs are wrong or missing; it is
not where a plugin publishes itself. If you find yourself editing it to make
your plugin usable, the fix belongs in your `plugin.json`.

### Operator-side prompt layering

Persona and facts belong to the operator, not to plugins. The `agent`
plugin resolves them, in order:

| Layer | Source | Override |
|---|---|---|
| identity / personality / channel | `const` in `llm.rs` | `<prompts_dir>/_identity.md`, `_personality.md`, `_channel.md` |
| facts (ids, account names, who-is-who) | none | `<prompts_dir>/_facts.md`, or `AGENT_PLUGIN_FACTS` |
| per-group behavior | built-in default | `<prompts_dir>/<group>.md`, then `AGENT_PLUGIN_PLUGIN_PROMPTS` (wins) |

`<prompts_dir>` is `AGENT_PLUGIN_PROMPTS_DIR`. Group names are derived from
the tool-name prefix (`tg_*` → `telegram`); `/` in a group name becomes `-`
in the filename.

### Why plugins do not ship behavior prompts

A plugin supplying free text that lands in the agent's system prompt could
instruct the agent to misuse *other* plugins' tools — read a credential via
`secret_get`, send it via `http_request`. That is a much larger blast radius
than exposing an action, which stays bounded by
`AGENT_PLUGIN_ALLOWED_ACTIONS`. If this capability is added later it needs,
at minimum: a per-plugin opt-in allowlist that is default-deny like the
action allowlist, a hard length cap, and source-attributed fencing around
the injected text so it is never mistaken for host instruction. Until that
design exists, behavior prompts stay operator-owned.
