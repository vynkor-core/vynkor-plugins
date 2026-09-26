# telegram plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

The original P0 plan is in `PLANS.md` (historical).

## Next

- **Vault-first credentials** — `api_hash` (and optionally the session
  itself) via `secrets`' `secret_get`; the plugin already declares
  `PERMISSION_SECRETS` but reads env only.
- **Safer session default** — default `TELEGRAM_PLUGIN_SESSION_DIR` to
  `$XDG_DATA_HOME/vyn/telegram` (0700) instead of `/tmp`, and expand `~`;
  refuse a world-readable session dir.
- **STAT-01** — `status` → `telegram_status`.
- **Bot client** (`telegram-bot`, root `ROADMAP.md` Planned) is a separate
  plugin: two-way chat with `agent` through the Bot API, not this
  user-account client.
