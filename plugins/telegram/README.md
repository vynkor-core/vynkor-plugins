# telegram plugin

Full MTProto user-client for vynkor (N-account, Rust + Grammers). Acts as your delegate — the `agent` plugin can read/write/search from your personal and corporate accounts.

## Actions

| Action | Params | Result |
|---|---|---|
| `status` | `{}` | `{version, accounts[], default_account, uptime_ms, engine_ready}` |
| `tg_list_dialogs` | `{account?, limit? (default 20, max 100)}` | `{account, dialogs: [{peer, unread}], total}` |
| `tg_get_history` | `{account?, peer (required), limit?}` | `{peer, messages: [{id, text, date, outgoing}], total}` |
| `tg_get_message` | `{account?, peer, id (required)}` | `{found, message?}` |
| `tg_search` | `{account?, query (required), limit?}` | `{query, messages: [{id, text, chat, date}], total}` |
| `tg_send_message` | `{account?, peer, text (required, 1..4096), reply_to?}` | `{peer, message_id}` |

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
