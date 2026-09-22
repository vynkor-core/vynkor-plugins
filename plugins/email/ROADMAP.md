# email plugin roadmap

Goal: give any vynkor plugin one blessed path to send an SMTP email, with the
password in one place (the `secrets` vault) instead of every plugin rolling its
own SMTP client.

## Decision: own SMTP socket, secrets for the password

`email` opens its own SMTP connection via `lettre` — it does **not** route
through `network`'s `http_request`, because SMTP is not HTTP. For the
credential it is vault-first, identical to `search`/`ai`/`tts`/`stt`: it calls
the kernel-routed `secret_get` action (owned by the `secrets` plugin) via
`VynkorClient::send_action`, with the process environment as fallback. Because
`secret_get` is gated by `PERMISSION_SECRETS`, and the kernel's
anti-laundering check (T-19) requires the *caller* to hold a gated action's
permission as well as the provider, `email` declares `"permissions":
["secrets"]` (Manifest v2 per-action `permission` on `secret_get`).

`email` declares **no** `PERMISSION_NETWORK` — it does not call
`network`'s `http_request`, so it runs with `sandbox: false` (real egress).

## Naming

Plugin id: `email`. Binary: `email`. Env-var prefix `EMAIL_PLUGIN_*` keeps the
established spelling (the vynkor rename doesn't touch protocol/config
surfaces).

## v0.1 (shipped, stays 0.1.0)

- Two actions, `email_send` + `email_list`:
  - `email_send`: fields `to` (required), `from`/`subject`/`body`/`is_html`,
    `credentials_env` (required, allowlisted via
    `EMAIL_PLUGIN_ALLOWED_CRED_ENVS`), `smtp_host`/`smtp_port`/`smtp_user`,
    `timeout_ms`. Vault-first via `src/key_resolve.rs`, `lettre` 0.11
    `AsyncSmtpTransport::relay`, `EMAIL_PLUGIN_SMTP_STUB=true` stub.
  - `email_list`: fields `imap_host`/`imap_port`/`imap_user`/`credentials_env`/`mailbox`/`limit`/`timeout_ms`. Same vault-first allowlist. Stub mode (`EMAIL_PLUGIN_IMAP_STUB=true` or `SMTP_STUB`) returns fake `emails[]` offline; real IMAP path (TLS `LOGIN`/`SELECT`/`FETCH`) is wired and returns `stubbed:false` when stub is off (requires `imap`/`native-tls` live host).
- Strict parse-time validation: `to` must contain `@`+`.` after, `subject` 1-200, `body` ≤10000, `mailbox` 1-100 no traversal, `limit` 1-50, ports non-zero.
- Testing: `request.rs` unit tests (validation + allowlist, 30 tests) and a fake-kernel `UnixStream::pair` integration test driving both handlers end to end (9 tests).

## v0.2 (in progress)

- **Reply-To / CC / BCC** — shipped. `email_send` accepts optional `cc`
  (array), `bcc` (array), `reply_to` (single address), each validated with
  the same `is_valid_email` check as `to`/`from`. `cc` becomes an ordinary
  `Cc:` header (visible to all recipients); `bcc` is added as an
  envelope-only recipient via `lettre`'s `.bcc()` — it is never written to
  any header of the sent message, verified by a test that asserts the
  address does not appear in the formatted output at all.
- **`email_list`** — outbox listing backed by `database` (or the `secrets`
  plugin for a sent-log), so callers can query what was sent. Deferred until
  a caller needs it.
- **Attachments** — MIME multipart with base64 bodies. Needs `lettre`'s
  `builder`/mime support and an operator cap on total payload size.
- **Provider abstraction** — optional: a trait over `lettre` vs a
  `network`-routed HTTP email API (Resend/Postmark/SendGrid), mirroring
  `search`'s provider adapters, if an HTTP-only deployment is needed.
- **Retry/backoff** — mirror `network`'s retry-on-429-equivalent (SMTP 421/450)
  with per-account backoff tuning.

## Non-goals / follow-ups

- **No inbound email (IMAP/POP3)** — `email` sends only.
- **No templating** — the caller builds the subject/body; `email` transmits
  them verbatim.
- **No secret logging** — the resolved password is never logged, cached to
  disk, or embedded in any error string; the stub response carries only
  non-secret debug fields.
- **No kernel special-casing for "email"** — an ordinary plugin like any other.
