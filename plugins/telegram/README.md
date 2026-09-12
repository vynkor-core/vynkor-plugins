# telegram plugin

Full MTProto user-client for vynkor (N-account, Rust + Grammers). Acts as your delegate — the `agent` plugin can read/write/search from your personal and corporate accounts.

## Prototype scope (P0)

| Action | Params | Result |
|---|---|---|
| `status` | `{}` | `{version, accounts[], default_account}` |
| `tg_list_dialogs` | `{account?, limit?, offset?, filter?}` | `{account, dialogs: [{peer, title, unread}]}` |
| `tg_get_history` | `{account?, peer, limit?, offset_id?}` | `{peer, messages: [...]}` |
| `tg_get_message` | `{account?, peer, id}` | `{found, message?}` |
| `tg_search` | `{account?, query, peer?, limit?}` | `{query, messages, total}` |
| `tg_send_message` | `{account?, peer, text, reply_to?}` | `{peer, message_id}` |

`peer = "self"` → Saved Messages. `peer = "@username"` | `"-100..."` | `"personal:123"` (via `account` selector).

Events: `plugin.telegram.new_message`, `plugin.telegram.message_sent` (publish requires `PERMISSION_EVENT_PUBLISH`).

## Config

```bash
TELEGRAM_PLUGIN_ACCOUNTS=personal,corporate
TELEGRAM_PLUGIN_API_ID_personal=12345
TELEGRAM_PLUGIN_API_HASH_personal=abc...
TELEGRAM_PLUGIN_PHONE_personal=+998...
TELEGRAM_PLUGIN_API_ID_corporate=...
TELEGRAM_PLUGIN_SESSION_DIR=~/.local/share/vyn/telegram
```

Secrets resolved vault-first via `secrets` plugin; env is fallback. Session files: `$SESSION_DIR/<account>.session` (0600).

## Architecture

Single-reader loop owns `VynkorClient`; handlers use `Rpc` proxy channel — see `docs/PLUGIN_AUTHORING.md §1`. MTProto via `grammers-client` + `antiban` (token bucket 8 rps, FloodWait jitter, proxy).

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

P1: `tg_edit/delete/forward/react/pin` with `requires_confirmation` gate (device.* confirms).
P2: media upload/download via `filesystem` jail, vector-db RAG, infection graph.

## Dev scripts

Helper scripts for manual testing (not shipped, dev-only):
- `get_messages.py` — fetch message history via Telethon (Python fallback for quick testing)
- `read_messages.py` — read recent messages from a dialog
- `run_test.sh` — run integration tests against a live Telegram account

See `PLANS.md` for full plan and `../../docs/PLUGIN_AUTHORING.md` for loop pattern.
