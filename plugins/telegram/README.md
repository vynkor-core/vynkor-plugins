# telegram plugin

Full MTProto user-client for vynkor (N-account, Rust + Grammers). Acts as your delegate — the `agent` plugin can read/write/search from your personal and corporate accounts.

## Actions

| Action | Params | Result | Confirmation |
|---|---|---|---|
| `status` | `{}` | `{version, accounts[], default_account, uptime_ms, engine_ready}` | — |
| `tg_list_dialogs` | `{account?, limit?, query?, unread_only?}` | `{account, dialogs: [{peer, peer_type, id, name, username, phone, unread_count, unread_mentions}], total, matched?}` | — |
| `tg_get_history` | `{account?, peer, limit?}` | `{peer, messages: [...], total}` | — |
| `tg_get_message` | `{account?, peer, id}` | `{found, message?}` | — |
| `tg_search` | `{account?, query, limit?}` | `{query, messages: [...], total}` | — |
| `tg_send_message` | `{account?, peer, text, reply_to?, no_typing?}` | `{peer, message_id}` — auto typing before send (10ms/char) | — |
| `tg_send_action` / `tg_set_typing` | `{account?, peer, action, progress?, duration_ms?}` | `{peer, action, duration_ms}` | — |
| `tg_list_contacts` | `{account?, limit?, query?}` | `{account, contacts: [{peer, name, phone, username}], total}` | — |
| `tg_get_contact` | `{account?, peer}` | `{peer, name, phone, username, is_contact, ...}` | — |
| `tg_list_unread` / `tg_get_unread` | `{account?, limit?, include_messages?, message_limit?}` | `{account, dialogs: [{peer, name, unread_count, unread_messages: [...] }], total_unread_dialogs, total_unread_messages}` | — |
| `tg_mark_read` / `tg_mark_all_read` | `{account?, peer?, max_id?}` | `{peer, marked, max_id} / {marked, errors}` | — |
| `tg_get_chat_info` | `{account?, peer}` | `{peer, name, username, phone, peer_type, ...}` | — |
| `tg_get_user` | `{account?, peer}` | `{peer, name, first_name, last_name, username, phone, ...}` | — |
| `tg_edit_message` | `{account?, peer, message_id, text}` | `{peer, message_id, edited}` | ⚠️ |
| `tg_delete_message` | `{account?, peer, message_id}` | `{peer, message_id, deleted}` | ⚠️ |
| `tg_forward_message` | `{account?, from_peer, to_peer, message_id}` | `{from_peer, to_peer, new_id}` | ⚠️ |
| `tg_add_reaction` | `{account?, peer, message_id, reaction}` | `{peer, message_id, reaction}` | — |
| `tg_pin_message` | `{account?, peer, message_id, silent?}` | `{peer, message_id, pinned}` | ⚠️ |
| `tg_upload_media` | `{account?, peer, file_path, caption?}` | `{peer, message_id, file_path}` | — |
| `tg_download_media` | `{account?, peer, message_id, output_path}` | `{peer, message_id, output_path}` | — |

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
```

Secrets resolved vault-first via `secrets` plugin; env is fallback.

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

P1: `tg_edit/delete/forward/react/pin` with `requires_confirmation` gate.
P2: media upload/download via `filesystem` jail, vector-db RAG.

## Dev scripts

Helper scripts for manual testing (not shipped, dev-only):
- `get_messages.py` — fetch message history via Telethon
- `read_messages.py` — read recent messages
- `run_test.sh` — run integration tests

See `PLANS.md` for full plan and `../../docs/PLUGIN_AUTHORING.md` for loop pattern.
