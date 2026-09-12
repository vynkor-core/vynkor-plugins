# Telegram Full-Client for Vynkor — Plan (English)

> Prototype: Rust + Grammers, inside `vyn` kernel. N-account (personal + corporate). P0 = read/write/search + status; media deferred to P2. Spec: `vynkor-wire/proto/vynkor_protocol.proto` v1.7 is single source of truth.

---

## TL;DR

**What you'll get:** A `telegram` plugin (`telegram-plugin` crate, binary `telegram`) running as a supervised process under `vyn`. It connects to Telegram via MTProto (Grammers) with N sessions and exposes `status`, `tg_list_dialogs`, `tg_get_history`, `tg_get_message`, `tg_search`, `tg_send_message` as kernel-routed actions. The `agent` plugin can discover them via `list_plugins/get_manifest` and act as your delegate (read Saved Messages, find conversations, send replies, search). Events `plugin.telegram.new_message` flow on the bus.

**Why this approach:** Dumb kernel + smart plugin. Kernel only routes 44-byte frames (UDS, HMAC, zstd≥64KiB, fragmented) — all Telegram complexity lives in the plugin, reusing `tg-swarm`'s proven antiban (token bucket 8 rps, FloodWait jitter, proxy, random device_model). Single-reader loop + RPC proxy prevents `send_action` frame loss (PLUGIN_AUTHORING §1). Python Telethon would be faster to prototype but Rust/Grammers matches `tg-swarm` and yields a 5MB musl binary with shared Parquet/dict compression later.

**What it will NOT do (prototype):** No media upload/download, no edit/delete/forward/react/pin, no Stories/calls, no TDLib, no mass invite, no vector-db RAG in P0 (hooks prepared, wired in P2).

**Effort:** Medium (3–5 days for P0, +1 week for P1/P2)
**Risk:** Medium — MTProto FloodWait/ban + session secret handling + single-reader concurrency
**Decisions to sanity-check:** Rust/Grammers over Python/Telethon; P0 action set; N-account via `TELEGRAM_PLUGIN_ACCOUNTS` env; confirmation gate deferred to P1.

---

## Scope

### Must have (P0 — this delivery)

1. **Crate & runtime** — `plugins/telegram/Cargo.toml`, `src/lib.rs` (Config, Rpc, handle_action), `src/main.rs` (single-reader `serve` + `VynkorClient::connect_from_env` + `register_full`), `README.md`.
2. **Manifest** — `PluginManifest{ permissions: [NETWORK, STORAGE, SECRETS, EVENT_PUBLISH], actions: [status, tg_list_dialogs, tg_get_history, tg_get_message, tg_search, tg_send_message] }`, `status` per INF-07 (`version, uptime_ms, engine_ready, last_error, counters`).
3. **N-account config** — `TELEGRAM_PLUGIN_ACCOUNTS=personal,corporate`, per-account `TELEGRAM_PLUGIN_API_ID_<UPPER>`, `_API_HASH_<UPPER>`, `_PHONE_<UPPER>`, `TELEGRAM_PLUGIN_SESSION_DIR` (default `~/.local/share/vyn/telegram`), fallback to `TG_API_ID/HASH`. Session files `<dir>/<account>.session` 0600, `api_id/hash` vault-first via `secrets` (env fallback). `account` param on every action selects session; missing → `default_account`.
4. **MTProto layer** — `src/mtproto/session.rs` (Grammers `Client` per account, `grammers-session` persistence), `src/mtproto/antiban.rs` (token bucket 8 rps/account, `FloodWaitError.seconds` sleep + jitter, circuit breaker, proxy pool stub, `device_model` randomization), `src/mtproto/client.rs` (connect, `get_dialogs`, `get_history`, `get_message`, `search`, `send_message`).
5. **Action handlers** — validation-first, exact error strings:
   - `tg_list_dialogs {account?, limit? (default 20, max 100), offset?, filter?}` → `{account, dialogs:[{peer,title,username,unread,peer_type}], total}`
   - `tg_get_history {account?, peer (required, "self"|"@name"|"-100..."), limit? (default 20, max 100), offset_id?}` → `{peer, messages:[{id,date,text,media_type}]}`; `peer=self` resolves to `Self`
   - `tg_get_message {account?, peer, id (required)}` → `{found, message?}`
   - `tg_search {account?, query (required, non-empty), peer?, limit?}` → `{query, messages, total}` (Grammers `messages.search`)
   - `tg_send_message {account?, peer, text (required, 1..4096), reply_to?}` → `{peer, message_id}` + fire-and-forget `EventPublish{plugin.telegram.message_sent}`
   - `status {}` → INF-07 JSON
6. **Events** — `plugin.telegram.new_message` on every live update (Grammers `updates` stream), published via `EventPublish` with `EVENT_PUBLISH_OK` check; `message_sent` on send. Subscribers: `agent`, `automations`.
7. **Storage cache (thin)** — per-caller `database` KV via Rpc proxy (`db_get/set/keys` for dialog cache, optional), no local SQLite in P0.
8. **Testing** — fake-kernel harnesses over `UnixStream::pair()` + `VynkorClient::from_stream`, `FakeDb` + `Published` recorder, polling helpers (response before event). Tests run without live Telegram.
9. **Ops** — `plugins.d/telegram.yaml` drop-in example, `VYN_JWT_SECRET + VYNKOR_JWT_TOKEN (sub=telegram)` docs, `WAYLAND_DISPLAY/DBUS_SESSION_BUS_ADDRESS` note, `cargo test --all`, `cargo clippy -D warnings`.

### Must NOT have (guardrails)

- No media (`tg_upload_media`, `tg_download_media`, `AudioStreamChunk`) — P2.
- No `edit/delete/forward/react/pin/join/leave` — P1 with `ConfirmationGate` (`requires_confirmation`, `device.*` allowlist).
- No vector-db RAG, no infection graph (`chat1→chat2`), no Tailscale swarm in P0 (interfaces stubbed).
- No new kernel changes, no proto bump, no `ipc_targets` wildcard (exact match only, T-04).
- No `as any`/`@ts-ignore`-class suppression, no `unwrap` on I/O, no hardcoded `api_id`.

---

## Verification Strategy

**Test decision:** tests-after (TDD would mock Grammers too early) + agent-executed QA. Framework: `cargo test` (tokio), `cargo clippy`, `cargo fmt --check`.

**Evidence:** `.omo/evidence/telegram/<attempt>/task-<N>-<slug>.log` (or `.omo/evidence/` outside ulw-loop). Each todo produces `task-N.log` with `cargo test -p telegram-plugin -- --nocapture` output.

**Scenarios per todo:**
- Happy: `status` returns version/accounts; `tg_send_message` with valid `peer=self,text=hello` → `{message_id}` + `plugin.telegram.message_sent` event (poll 2s).
- Failure: `tg_send_message {text:""}` → `ACTION_ERROR "text required"`; `tg_search {query:""}` → error; `tg_get_message {id:0}` → error; unknown action → `unknown action`; `FloodWait` → handler sleeps jitter, not panic.
- Concurrency: two `tg_search` while `tg_get_history` is in-flight — both replies arrive (RPC proxy proves no frame loss).

**Live kernel gate:** `scripts/live-audit/vynkor_ws.py` pattern — must target `kernel` (not slug direct), HMAC + `wss://` TLS, per-plugin JWT.

---

## Execution Strategy

### Parallel waves

- **Wave 0 — Foundation:** Todo 1 (crate), Todo 2 (config N-account), Todo 3 (manifest + status + single-reader loop) — sequential (1 blocks 2,3) but 2&3 can parallelize after 1.
- **Wave 1 — MTProto:** Todo 4 (session + antiban), Todo 5 (handlers P0) — parallel after Wave 0 (both need crate, neither blocks the other except antiban is used by handlers — handlers stub antiban initially).
- **Wave 2 — Events & Cache:** Todo 6 (live updates + EventPublish), Todo 7 (integration tests / fake kernel) — parallel.
- **Wave 3 — Polish:** Todo 8 (README, drop-in, clippy/fmt, evidence) — after all.

### Dependency matrix

| Todo | Depends on | Blocks | Can parallelize with |
|---|---|---|---|
| 1. Crate scaffold | — | 2,3,4,5 | — |
| 2. N-account Config | 1 | 4,5 | 3 |
| 3. Manifest + status + loop | 1 | 5,6 | 2 |
| 4. MTProto session/antiban | 2 | 5 | 3 |
| 5. P0 handlers | 3,4 | 6,7 | — |
| 6. Events (live) | 5 | 7 | 7 |
| 7. Tests (fake kernel) | 5,6 | 8 | 6 |
| 8. Docs & QA | 7 | — | — |

---

## Todos

- [ ] 1. Scaffold telegram crate (Cargo.toml, lib.rs, main.rs, README)
  What to do: Create `plugins/telegram/` with `Cargo.toml` (vynkor-sdk 0.0.3, vynkor-wire 0.0.3, tokio full, serde, grammers-* 0.7, chrono, anyhow, thiserror, tracing), `src/lib.rs` (Config, Rpc, HandleResult, handle_action stubs), `src/main.rs` (PLUGIN_ID=telegram, PLUGIN_VERSION=0.1.0, manifest(), unix_millis(), action_response(), event_envelope(), serve() single-reader select! over client.recv/outbound_rx/rpc_rx, main() tracing + connect_from_env).
  Must NOT do: No business logic beyond stubs; no unwrap on env; no hardcoded api_id.
  Parallelization: Wave 0 | Blocked by: — | Blocks: 2,3
  References: `plugins/notes/src/main.rs:1-212` (loop pattern), `plugins/notes/Cargo.toml`, `plugins/calendar/Cargo.toml`, `vynkor-sdk-python/README.md:1-52` (Plugin trait)
  Acceptance: `cargo check -p telegram-plugin` passes; `cargo test -p telegram-plugin -- status_returns_accounts` passes (pure handle_action).
  QA: `cargo test -p telegram-plugin -- --nocapture` happy (status) + failure (unknown action → error string contains "unknown action") → evidence `.omo/evidence/telegram/task-1.log`
  Commit: N (scaffold only)

- [ ] 2. N-account Config (env + vault-first)
  What to do: `src/lib.rs::Config::from_env()` parses `TELEGRAM_PLUGIN_ACCOUNTS`, per-account `TELEGRAM_PLUGIN_API_ID_<UPPER>/_API_HASH/_PHONE`, `TELEGRAM_PLUGIN_SESSION_DIR`, fallback `TG_API_ID/HASH`. `AccountConfig {id, api_id, api_hash, phone, session_path}`. Tests for single account, two accounts (personal+corporate), missing api_hash → skipped.
  Must NOT do: No plaintext logging of api_hash/phone; no session file creation here.
  Parallelization: Wave 0 | Blocked by: 1 | Blocks: 4,5 | With: 3
  References: `tg-swarm/config/swarm.toml`, `vynkor-wire/proto/vynkor_protocol.proto:69-92` (PluginRegister), `plugins/secrets/README.md` (vault-first pattern)
  Acceptance: `cargo test -p telegram-plugin config_parses_two_accounts` — sets env via OnceLock helper, asserts `cfg.accounts.len()==2 && cfg.default_account=="personal"`.
  QA: happy (two accounts) + failure (no env → empty accounts, default_account=="default") → task-2.log
  Commit: N

- [ ] 3. Manifest + status + single-reader loop wiring
  What to do: `manifest()` returns `PERMISSION_NETWORK/STORAGE/SECRETS/EVENT_PUBLISH` + 6 actions. `handle_action` for `status` returns `{version, accounts, default_account, uptime_ms, engine_ready}` via `vynkor_sdk::status::status_response` helper. Wire `serve()` pending map `action_id -> oneshot`, outbound channel, `register_full` with `VYN_JWT_TOKEN`, Ping/Pong, PluginShutdown, EventAck, ActionResponse dispatch.
  Must NOT do: No direct `client.send_action` outside loop; no `ipc_targets` (kernel-routed only).
  Parallelization: Wave 0 | Blocked by: 1 | Blocks: 5,6 | With: 2
  References: `plugins/notes/src/main.rs:31-40` (manifest), `plugins/notes/src/main.rs:76-204` (serve), `docs/PLUGIN_AUTHORING.md:1-50` (single-reader), `docs/PLUGIN_AUTHORING.md:145-172` (status INF-07)
  Acceptance: Fake-kernel test `status_via_fake_kernel` — `UnixStream::pair`, shim answers `PluginRegisterAck{accepted:true}`, calls `status` → `{version:"0.1.0"}`.
  QA: happy (status) + failure (registration rejected → PermissionDenied) → task-3.log
  Commit: Y | feat(telegram): manifest status and single-reader loop

- [ ] 4. MTProto session + antiban (Grammers)
  What to do: `src/mtproto/session.rs` — `SessionPool { clients: HashMap<account_id, Client> }`, `connect(account)` via `grammers-client` + `grammers-session::Session::load_file_or_create`, `is_connected()`, `disconnect()`. `src/mtproto/antiban.rs` — `Antiban { bucket: TokenBucket { capacity: 8, rate: 8.0 }, proxy_pool, device_model }`, `check(account) -> Result<()>`, `on_flood_wait(seconds)` sleep + jitter 0.2..0.8s + circuit breaker. Unit tests with mocked time.
  Must NOT do: No real Telegram connection in unit tests; no `unwrap` on session file.
  Parallelization: Wave 1 | Blocked by: 2 | Blocks: 5 | With: 3 (logic independent)
  References: `tg-swarm/src/antiban/*`, `tg-swarm/docs/ANTIBAN.md`, `tg-swarm/src/swarm/session_pool.rs`, `grammers` docs (0.10 -> 0.7 compat check)
  Acceptance: `cargo test -p telegram-plugin antiban_token_bucket_blocks_on_exhaustion` — 9th call within 1s → `Err(RateLimited)`, after 1s → Ok.
  QA: happy (8 rps allowed) + failure (FloodWait 60s → sleep counted, not panic) → task-4.log
  Commit: Y | feat(telegram): mtproto session pool and antiban

- [ ] 5. P0 action handlers (list_dialogs, get_history, get_message, search, send_message)
  What to do: Implement `handle_action` branches with validation (non-empty peer/query/text, text≤4096, limit clamp 1..100, id!=0). Each handler resolves `account` param → `SessionPool::get(account)`, calls `antiban.check()` then Grammers (`client.get_dialogs()`, `client.get_messages(peer, limit)`, `client.search(peer, query)`, `client.send_message(peer, text)`). `tg_send_message` also queues `EventToPublish{plugin.telegram.message_sent}`. All via `Rpc` proxy if they need `database` later (stub now). Error mapping: `FloodWait` → `Err("rate limited, retry after Xs")` (ACTION_ERROR), not crash.
  Must NOT do: No `panic!` on invalid peer; no silent truncation of text >4096 (error instead); no direct `VynkorClient` use inside handler.
  Parallelization: Wave 1 | Blocked by: 3,4 | Blocks: 6,7
  References: `plugins/notes/src/lib.rs` (handle_action dispatch), `tg-swarm/src/collector/*` (history/search), `vynkor-wire/proto/vynkor_protocol.proto:221-285` (ActionRequest/Response), `docs/PLUGIN_AUTHORING.md:49-65` (kernel routing facts, per-action permission)
  Acceptance: `cargo test -p telegram-plugin` — `tg_search_requires_query` fails with "query required"; `tg_send_message_validates_text_len` fails with "text >4096"; `tg_get_history_self_resolves` returns peer=self with stub messages.
  QA: happy (send with peer=self) + failure (empty text, bad peer format) + concurrency (spawn 3 handle_action concurrently, all respond) → task-5.log
  Commit: Y | feat(telegram): p0 handlers read/write/search

- [ ] 6. Live updates + EventPublish
  What to do: `src/events/mod.rs` — spawn `live_listener` task that subscribes to Grammers `Client::updates()` stream, on `NewMessage` push through `Rpc` proxy to `handle_action`-less path: directly `outbound_tx.send(event_envelope("plugin.telegram.new_message", payload))`. Register `EventPublish` ack handling. Add `subscribe` on kernel side if needed (plugin subscribes to nothing inbound, only publishes).
  Must NOT do: No `send_action` inside listener (only EventPublish); no unbounded channel (cap 64, drop oldest with log).
  Parallelization: Wave 2 | Blocked by: 5 | Blocks: 7 | With: 7
  References: `plugins/notes/src/main.rs:123-150` (event ack), `plugins/calendar/src/main.rs` (timer + publish), `vynkor-wire/proto/vynkor_protocol.proto:314-360` (Event, EventPublish), `plugins/notify/README.md` (push_send pattern)
  Acceptance: Fake-kernel test `live_event_published` — shim records `plugin.telegram.new_message` within 2s of simulated Grammers update.
  QA: happy (event arrives) + failure (EventPublishAck PERMISSION_DENY → log, not crash) → task-6.log
  Commit: Y | feat(telegram): live updates and event publishing

- [ ] 7. Integration tests (fake kernel, no live Telegram)
  What to do: `src/main.rs::tests` — `FakeDb` + `Published` + `Shim` (copy notes pattern: registration handshake FIRST, buffered `mpsc` commands, pending map, `handle db_*` if needed). Tests: `status_returns_accounts`, `unknown_action_errors`, `concurrent_search_while_history` (proves RPC proxy), `event_published_before_response_race` (poll helper). `#[tokio::test]` with `OnceLock` env helper for TELEGRAM_PLUGIN_* (no races).
  Must NOT do: No live network; no `std::env::set_var` without OnceLock; no sleep longer than 5s in test.
  Parallelization: Wave 2 | Blocked by: 5,6 | Blocks: 8 | With: 6
  References: `plugins/notes/src/main.rs:214-638` (full fake-kernel harness), `plugins/search/src/main.rs` (OnceLock env), `docs/PLUGIN_AUTHORING.md:67-88` (fake kernel)
  Acceptance: `cargo test -p telegram-plugin -- --nocapture` all green, `cargo clippy -p telegram-plugin -- -D warnings` clean.
  QA: 4 tests happy + 3 failure paths + 1 concurrency → task-7.log
  Commit: Y | test(telegram): fake-kernel integration and concurrency

- [ ] 8. Docs, drop-in, QA, evidence
  What to do: Fill `README.md` (actions table, config, peer formats, examples), `plugins.d/telegram.yaml.example` (jwt_secret, token sub=telegram, env), `ROADMAP.md` (P1/P2 deferred), run `cargo fmt --check`, `cargo clippy`, collect `.omo/evidence/telegram/*`.
  Must NOT do: No secrets in repo; no `unwrap` left; no `println!` without prefix.
  Parallelization: Wave 3 | Blocked by: 7 | Blocks: —
  References: `plugins/notes/README.md`, `plugins/calendar/README.md`, `vynkor/docs/VYN_PRODUCT_LAYOUT.md` (paths), `docs/PLUGIN_AUTHORING.md:174-212` (live kernel audit tips)
  Acceptance: `cargo fmt --check` + `cargo clippy -D warnings` + `cargo test` all pass; `PLANS.md` and `.omo/plans/telegram-full-client.md` present.
  QA: `cargo test -p telegram-plugin` + `cargo clippy` → task-8.log
  Commit: Y | docs(telegram): readme drop-in and qa

---

## Final Verification Wave

- [ ] F1. Plan compliance audit — every Must-have has a todo, every Must-NOT-have is absent from code.
- [ ] F2. Code quality — `cargo fmt --check`, `clippy -D warnings`, no `unwrap` on I/O, no `as any` suppression.
- [ ] F3. Real manual QA — `VynkorClient::from_stream` fake kernel: `status`, `tg_search`, `tg_send_message` + event poll, concurrent 3-way.
- [ ] F4. Scope fidelity — no media, no edit/delete/react, no vector-db, no kernel/proto changes.

---

## Commit Strategy

- Atomic commits per todo (type(scope): summary), `feat` for code, `test` for harness, `docs` for readme. No squashing until green F1-F4.

## Success Criteria

- `cargo test -p telegram-plugin` green (≥8 tests), `clippy` clean, `fmt` clean.
- `telegram` binary registers with live `vyn` (manual `cargo run -p telegram-plugin` with `VYN_SOCKET_PATH` + `VYN_JWT_TOKEN`), `status` returns N accounts, `tg_list_dialogs`/`tg_search`/`tg_send_message` round-trip via `wss` `send_action("telegram", ...)`.
- Events `plugin.telegram.new_message` observable on bus.
- Session files 0600, no secret leakage, FloodWait handled without crash.

---

## Highlights / Isyuminki

- **N-account isolation:** `account` param on every call, `peer` syntax `personal:self` or `corporate:@channel`, separate `TokenBucket` per account — personal and corporate never share rate limits.
- **Agent-native:** `action_specs` with JSON Schema + `ActionRisk` let `agent` auto-discover tools via `get_manifest`; `PERMISSION_EVENT_PUBLISH` makes Telegram a first-class event source for `automations` (`new_message` → `ai` → `notify`).
- **Antiban reuse:** Direct port of `tg-swarm`'s battle-tested flood logic — 379MB→41MB compression lineage, not a toy.
- **Phone as confirmer:** P1 gate uses `device.*` (Android) to confirm `HIGH` actions — your phone is the second factor.
- **5MB musl:** Single static binary, Tailscale-ready for VPS+Pi swarm later.

## Pros / Cons

**Pros:** Full user powers (Saved Messages, reactions in P1), supervised, HMAC+JWT, no kernel change, 5MB deploy, RAG-ready.
**Cons:** Session = account takeover risk (0600 + vault mandatory), FloodWait = latency spikes, Grammers 0.7 API churn vs tg-swarm's 0.10, env-var OnceLock testing complexity, live testing needs real Telegram creds (not CI).

## Risks & Mitigations

- Ban: 8 rps bucket + jitter + no mass actions in P0.
- Secret leak: vault-first, `secrets` plugin, `.gitignore` `*.session`, `chmod 600`.
- Frame loss: single-reader + RPC proxy, fake-kernel concurrency test.
- Live kernel mismatch (target `kernel` vs slug): harness asserts `client.send("kernel", …)`.

---

## Open Questions (Resolved for P0)

- Account `personal` vs `corporate` — via `TELEGRAM_PLUGIN_ACCOUNTS` CSV, not separate plugins (simpler supervision).
- Media — deferred to P2 via `filesystem` jail (canonicalize + symlink check, PLUGIN_AUTHORING §5).
- Language — Rust/Grammers (chosen) over Python/Telethon for consistency with `tg-swarm`.
