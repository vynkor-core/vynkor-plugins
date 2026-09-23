# telegram plugin — dev notes

Full MTProto user-client (Rust + Grammers) running as a supervised `vyn` kernel plugin. See `README.md` for the action/event reference; this file is operational/architectural notes for whoever (human or Claude) works in this directory next.

## Architecture in one paragraph

`main.rs` owns one `VynkorClient` (single-reader loop, see `../../docs/PLUGIN_AUTHORING.md` §1) and a `select!` loop that multiplexes: inbound kernel envelopes → `handle_action`, outbound responses, and a `global_event_rx` channel fed by one `spawn_live_listener` task per configured account (`src/events/mod.rs`). `lib.rs` holds all action handlers behind `handle_action`/`handle_action_inner`. `src/mtproto/` is the MTProto transport layer: `session.rs` (per-account `Client` pool + dialog cache) and `antiban.rs` (rate limiting + FloodWait handling + circuit breaker).

## Hard-won gotchas

- **Never spawn more than one live-update listener per account.** A double-spawn (one real, one to a dead-end drain) pegged the single-threaded tokio runtime at ~99% CPU and starved the action-handler `select!` arm — `tg_get_unread`/`tg_list_dialogs` timed out even though the plugin process looked "up". If you touch `main.rs`'s account-loop or `events::spawn_live_listener`, keep it to exactly one call per account, merged into `serve()`'s `select!`.
- **Kernel auto-namespaces published events** as `plugin.{sender_id}.{event_type}` (see `vynkor/src/ipc/protocol/router.rs:652` in the sibling `vynkor` repo). Always publish the *bare* event name (`"new_message"`, not `"plugin.telegram.new_message"`) — pre-prefixing double-namespaces it and nothing can subscribe to the result.
- **`UpdateStream::next()` (grammers) survives its own errors** — `&mut self`, internal `message_box` state intact after an `Err`. The listener loop in `events/mod.rs` relies on this: on error it backs off (1s→30s exponential) and retries the *same* stream. Don't reintroduce a `break` on error — that silently kills live updates for the rest of the process's life with no recovery. The only exit is `MAX_CONSECUTIVE_ERRORS` (20): the whole process exits so the supervisor respawns it cleanly.
- **`stack overflow` crash-loop every ~2 min = unreachable DC, not a code bug.** When the account's home DC can't be reached over TCP, grammers' sender reconnects endlessly and overflows a worker stack. Logs show only `connecting...` then the overflow. `SessionPool::connect()` now times out after 45s with a clear error instead. Fix the network: an alternate DC IP in the session's `dc_option` table, or `TELEGRAM_PLUGIN_PROXY_URL` (see README "Unreachable DC"). Bigger stacks and skipping catch-up were tried and don't help.
- **Plugin stderr isn't in `journalctl`.** It goes to the kernel's in-memory ring buffer, which resets on every respawn: `vyn plugin -c ~/.config/vyn/config.yaml --token "$(cat ~/.config/vyn/admin_token)" logs telegram`. Grab it right as the process dies. Set `RUST_LOG=info` in `plugins.d/telegram.yaml`, otherwise only errors are logged.
- **FloodWait is handled in exactly one place**: `handle_action` in `lib.rs` parses `FLOOD_WAIT (value: N)` out of any handler's returned error string and arms `Antiban::on_flood_wait()` for the resolved account. Don't add a second per-handler flood-wait path — the string-based interception at the single return point was a deliberate choice over threading a hook through ~28 handlers with different grammers call shapes.
- **Grammers itself auto-retries FloodWait once** for waits ≤60s (`ClientConfiguration::flood_sleep_threshold`, default 60s, set in `grammers-client`, not overridden here). Only waits that exceed that, or a second flood on the same call, actually propagate up to our antiban layer.
- **Circuit breaker has a cooldown** (5 min, `CIRCUIT_COOLDOWN` in `antiban.rs`) — it half-opens automatically, no process restart needed. If you change the threshold/cooldown constants, update the `mtproto::antiban::tests` (they assert exact trip/reset behavior using synthetic `Instant`s, no real sleeping).
- **`SessionPool::connect()` checks `is_authorized()`** before registering a client in the pool. A stale/expired session now fails connect with a clear message instead of silently registering a broken client (`status.engine_ready` would otherwise lie `true`).

## Deploy loop (manual, no CI wired for this plugin yet)

```bash
cargo build -p telegram-plugin --release
cargo test -p telegram-plugin --release   # 49 tests, keep green
/home/behzod/.local/bin/vyn stop -c ~/.config/vyn/config.yaml
\cp -f target/release/telegram ~/.local/lib/vyn/plugins/telegram/telegram   # \cp: cp is aliased -i interactively here, force it
/home/behzod/.local/bin/vyn start -c ~/.config/vyn/config.yaml
```

Account config lives in `~/.config/vyn/plugins.d/telegram.yaml` (`TELEGRAM_PLUGIN_ACCOUNTS`, per-account `API_ID`/`API_HASH`/`PHONE`, `TELEGRAM_PLUGIN_SESSION_DIR`). Session files are SQLite at `<session_dir>/<account>.session`.

## Retired: `tg-watcher`

There used to be a second, separate polling-based plugin (`plugins/tg-watcher/`, `TG_WATCHER_*` env vars, its own `plugins.d/tg-watcher.yaml`) that duplicated what the in-process live listener now does. It's fully removed (commit `bfa0997`) — if you see stray references to `TG_WATCHER_ENABLED` or a `tg-watcher` binary/process, that's leftover cruft from before the listener was fixed, not a real dependency.

## Testing without live Telegram

Unit tests in `lib.rs`/`antiban.rs`/`session.rs` use a fake-kernel harness (`UnixStream::pair()` + `VynkorClient::from_stream`, see `PLANS.md` §9) — no real MTProto connection needed. `cargo test -p telegram-plugin` should always pass without a configured account. For live smoke-testing against a real running kernel, use the plugin's JWT (from `plugins.d/telegram.yaml`'s `VYN_JWT_SECRET`/`VYN_JWT_TOKEN`) to call actions directly over `/run/user/1000/vyn.sock` via the Python SDK (`vynkor.VynkorClient`) — see recent session history for the call pattern if needed.
