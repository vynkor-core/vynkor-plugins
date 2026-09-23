# telegram plugin

Full MTProto user-client for vynkor (N-account, Rust + Grammers). Acts as your delegate — the `agent` plugin can read/write/search from your personal and corporate accounts.

## Actions

| Action | Params | Result | Confirmation |
|---|---|---|---|
| `status` | `{}` | `{version, accounts[], default_account, uptime_ms, engine_ready}` | — |
| `tg_list_dialogs` | `{account?, limit?, query?, unread_only?, fields?}` | `{account, dialogs: [{peer, name, unread_count}], total, matched?}` — `fields=minimal/full or peer,name,unread_count`, 10s cache | — |
| `tg_get_history` | `{account?, peer, limit?}` | `{peer, messages: [...], total}` | — |
| `tg_get_message` | `{account?, peer, id}` | `{found, message?}` | — |
| `tg_search` | `{account?, query, limit?}` | `{query, messages: [...], total}` | — |
| `tg_send_message` | `{account?, peer, text, reply_to?, no_typing?}` | `{peer, message_id}` — auto typing before send (10ms/char), invalidates dialog cache | — |
| `tg_send_action` / `tg_set_typing` | `{account?, peer, action, progress?, duration_ms?}` | `{peer, action, duration_ms}` | — |
| `tg_list_contacts` | `{account?, limit?, query?, fields?}` | `{account, contacts: [{peer, name, phone}], total}` — `fields`, 10s cache | — |
| `tg_get_contact` | `{account?, peer, fields?}` | `{peer, name, phone, ...}` — `fields`, 10s cache | — |
| `tg_list_unread` / `tg_get_unread` | `{account?, limit?, fields?, include_messages? (default false), message_limit?}` | `{account, dialogs: [{peer, name, unread_count, unread_messages?}], total_unread_dialogs}` — 10s cache | — |
| `tg_mark_read` / `tg_mark_all_read` | `{account?, peer?, max_id?}` | `{peer, marked, max_id} / {marked, errors}` — invalidates cache | — |
| `tg_get_chat_info` | `{account?, peer, fields?}` | `{peer, name, ...}` — `fields`, 10s cache | — |
| `tg_get_user` | `{account?, peer, fields?}` | `{peer, name, ...}` — `fields`, 10s cache | — |
| `tg_edit_message` | `{account?, peer, message_id, text}` | `{peer, message_id, edited}` | ⚠️ |
| `tg_delete_message` | `{account?, peer, message_id}` | `{peer, message_id, deleted}` | ⚠️ |
| `tg_forward_message` | `{account?, from_peer, to_peer, message_id}` | `{from_peer, to_peer, new_id}` | ⚠️ |
| `tg_add_reaction` | `{account?, peer, message_id, reaction}` | `{peer, message_id, reaction}` | — |
| `tg_pin_message` | `{account?, peer, message_id, silent?}` | `{peer, message_id, pinned}` | ⚠️ |
| `tg_upload_media` | `{account?, peer, file_path, caption?}` | `{peer, message_id, file_path}` | — |
| `tg_download_media` | `{account?, peer, message_id, output_path}` | `{peer, message_id, output_path}` | — |
| `tg_send_voice` | `{account?, peer, file_path, caption?}` | `{peer, message_id, file_path}` | — |
| `tg_send_sticker` | `{account?, peer, file_path}` | `{peer, message_id, file_path}` | — |
| `tg_send_animation` | `{account?, peer, file_path, caption?}` | `{peer, message_id, file_path}` | — |
| `tg_delete_messages` | `{account?, peer, message_ids[]}` | `{peer, deleted, errors}` | ⚠️ |
| `tg_forward_messages` | `{account?, from_peer, to_peer, message_ids[]}` | `{from_peer, to_peer, new_ids}` | ⚠️ |
| `tg_transcribe_voice` | `{account?, peer, message_id}` | `{peer, message_id, text}` — via `stt` plugin | — |

## Peer formats

| Format | Example | Description |
|---|---|---|
| `self` | `"self"` | Saved Messages |
| `user:<id>` | `"user:12345"` | User by ID |
| `chat:<id>` | `"chat:67890"` | Group chat by ID |
| `channel:<id>` | `"channel:11111"` | Channel by ID |
| `<negative_id>` | `"-100123456"` | Channel (Bot API format) |

## Events

| Event | Payload | Description |
|---|---|---|
| `plugin.telegram.new_message` | `{message_id, peer, sender, text, date}` | Incoming message |
| `plugin.telegram.message_sent` | `{peer, message_id}` | Message sent |

## Config

```bash
TELEGRAM_PLUGIN_ACCOUNTS=default,corporate
TELEGRAM_PLUGIN_API_ID_default=12345
TELEGRAM_PLUGIN_API_HASH_default=abc123...
TELEGRAM_PLUGIN_PHONE_default=+998...
TELEGRAM_PLUGIN_API_ID_corporate=...
TELEGRAM_PLUGIN_SESSION_DIR=~/.local/share/vyn/telegram
TELEGRAM_PLUGIN_PROXY_URL=socks5://127.0.0.1:1080   # optional, all accounts
```

Secrets resolved vault-first via `secrets` plugin; env is fallback.

## Antiban (`src/mtproto/antiban.rs`)

Per-account token bucket (8 rps burst capacity, 8/s refill) gates every action via `check_antiban()`. On a real `FLOOD_WAIT_X` from Telegram:

1. Grammers itself auto-sleeps once for waits ≤60s (`ClientConfiguration::flood_sleep_threshold`, default 60s) — transparent, no plugin code involved.
2. If it still surfaces as an error (wait >60s, or a second flood on the same call), `handle_action` catches it centrally by pattern-matching `FLOOD_WAIT (value: N)` out of the error string (one interception point for all ~28 handlers, not threaded through each) and calls `Antiban::on_flood_wait(account, n)` — sleeps a jittered `0.2..0.8 × n` seconds, increments the account's consecutive-flood counter.
3. Three consecutive flood-waits trip the circuit breaker: `check_antiban()` then fails fast (`antiban: circuit open for account X`) instead of hammering Telegram.
4. The breaker half-opens after a 5-minute cooldown — one request is let through; if it floods again the breaker re-trips and the cooldown clock restarts. It does **not** require a process restart to recover.

## Live updates & reconnect (`src/events/mod.rs`)

One `spawn_live_listener` task per account owns the whole `UpdateStream` and pushes `new_message`/other updates onto a channel merged into `main.rs`'s `serve()` select loop (single client per account, no duplicate listeners — a double-spawn here previously pegged the runtime at ~99% CPU and starved the action handler, see git history on `fix(telegram): stop CPU-starve loop...`).

`UpdateStream::next()` surfaces transient RPC errors (network blips, timeouts) without losing its internal state, so on error the listener backs off (1s, doubling, capped at 30s) and retries the *same* stream — it never permanently dies from a transient failure. After 20 consecutive errors it exits the process (`std::process::exit(1)`) so the supervisor respawns with a fresh connection instead of hanging indefinitely.

## Session auth (`src/mtproto/session.rs`)

`SessionPool::connect()` calls `client.is_authorized()` right after opening the session file and fails the connect (clear error, account not registered in the pool) if the session is stale/logged-out, instead of silently registering a broken client that would make every action fail opaque while `status` still reports `engine_ready: true`.

That check is bounded by a 45s timeout. If the account's home DC is unreachable, grammers otherwise reconnects forever and eventually aborts the process with `thread 'tokio-rt-worker' has overflowed its stack`, crash-looping under the supervisor. On timeout the sender runner is aborted and connect fails with `could not reach Telegram within 45s … set TELEGRAM_PLUGIN_PROXY_URL`.

### Unreachable DC (network blocks)

Some networks drop TCP to parts of Telegram's ranges while ICMP still passes (so `ping` looks fine). Check with `timeout 5 bash -c '</dev/tcp/IP/443'`. The DC addresses live in the session file's `dc_option` table (`dc_id`, `ipv4`, `ipv6`, `auth_key`); auth keys are per-DC, not per-IP. Two fixes:

1. **Alternate DC address**: repoint the blocked DC to another reachable IP of the same DC (e.g. DC5 `91.108.56.x` → `149.154.171.5:443`) with the plugin stopped. Back up the `.session` file first. Telegram's config refresh may rewrite it.
2. **Proxy**: set `TELEGRAM_PLUGIN_PROXY_URL` (SOCKS5, via grammers' `proxy` feature). Needs a full kernel restart to pick up the new env.

## Build

```bash
cargo build -p telegram-plugin --release
```

## Testing

```bash
cargo test -p telegram-plugin
cargo clippy -p telegram-plugin -- -D warnings
```

## Typing emulation (why 10ms/char)

`tg_send_message` теперь честно эмулирует "печатает..." перед отправкой — чтобы собеседник видел как у живого человека, и чтобы агент не мог соврать "отправил".

* Формула: `duration_ms = chars * 10` (Unicode `chars()`, не байты), с клиппингом `300ms ≤ duration ≤ 5000ms` и повтором `SetTyping` каждые `4s` (Telegram показывает "печатает" ~5с после одного `SetTyping`).
* Почему 10ms: требование продукта — ~100 символов/сек. `привет` = 6 chars → `6*10=60ms` (в ТЗ пример `0.6s` — это опечатка: для `0.6s` при 10ms нужно 60 символов; при 100ms/char было бы `6*100=600ms`). Оставили 10ms как в ТЗ, нижний порог 300ms делает короткие `привет` видимыми.
* Чтобы отключить: `tg_send_message {"peer":"...","text":"...","no_typing":true}`.
* Отдельно: `tg_send_action {"peer":"...","action":"typing","duration_ms":1200}` / `upload_photo` / `record_audio` / `cancel` напрямую через `messages.SetTyping` (`SendMessageAction`).

## Next (deferred)

P2: vector-db RAG over message history (hooks prepared, not wired).

See `PLANS.md` for full plan and `../../docs/PLUGIN_AUTHORING.md` for loop pattern.
