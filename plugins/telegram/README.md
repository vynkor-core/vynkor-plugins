# telegram plugin

Full MTProto user-client for vynkor (N-account, Rust + Grammers). Acts as your delegate — the `agent` plugin can read/write/search from your personal and corporate accounts.

## Actions

| Action | Params | Result | Confirmation |
|---|---|---|---|
| `status` | `{}` | `{version, accounts[], default_account, uptime_ms, engine_ready}` | — |
| `tg_list_dialogs` | `{account?, limit?}` | `{account, dialogs: [{peer, unread}], total}` | — |
| `tg_get_history` | `{account?, peer, limit?}` | `{peer, messages: [...], total}` | — |
| `tg_get_message` | `{account?, peer, id}` | `{found, message?}` | — |
| `tg_search` | `{account?, query, limit?}` | `{query, messages: [...], total}` | — |
| `tg_send_message` | `{account?, peer, text, reply_to?}` | `{peer, message_id}` | — |
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

## Next (deferred)

P1: `tg_edit/delete/forward/react/pin` with `requires_confirmation` gate.
P2: media upload/download via `filesystem` jail, vector-db RAG.

## Dev scripts

Helper scripts for manual testing (not shipped, dev-only):
- `get_messages.py` — fetch message history via Telethon
- `read_messages.py` — read recent messages
- `run_test.sh` — run integration tests

See `PLANS.md` for full plan and `../../docs/PLUGIN_AUTHORING.md` for loop pattern.
