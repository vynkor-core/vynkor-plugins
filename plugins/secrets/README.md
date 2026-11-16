# secrets plugin

Encrypted credential/API-key vault for Vynkor plugins, gated by
`PERMISSION_SECRETS`. One ChaCha20-Poly1305-encrypted vault file per calling
plugin — callers cannot see, read, or overwrite each other's secrets.

Identity is taken only from the kernel-stamped `ActionRequest.caller_plugin_id`,
never from params, and is sanitized to `[a-zA-Z0-9_-]` before use as a
filename — an empty or malformed caller id is an `ACTION_ERROR`, never a
shared/default namespace (same policy as the `database` plugin).

## Why this exists

`network`/`ai`/`tts`/`stt` currently keep provider API keys in plaintext
`config.yaml` env vars. `secrets` gives those — and future plugins like
`email`/`image`/`media` — a single, encrypted, per-caller place to keep
credentials, and the kernel gate (`PERMISSION_SECRETS`) means only callers
granted the permission can read a value back.

## Storage & encryption

- Vault file: `{SECRETS_PLUGIN_DATA_DIR}/{caller_plugin_id}.vault`, created
  with mode `0600` on first write.
- On-disk format: `magic "VYNKORVLT"` + version byte + 12-byte random nonce +
  ChaCha20-Poly1305 ciphertext of the JSON secrets map (the 16-byte AEAD tag
  is appended by the cipher). Every mutation re-encrypts the whole file.
- Writes are atomic: temp file → fsync → rename → fsync dir. A torn write can
  never leave a half-written vault.
- A tampered or corrupted vault fails decryption loudly (AEAD tag mismatch) —
  it is never silently reset or returned as empty.

## Master key

The vault is symmetric-key encrypted with a single 32-byte master key from
`SECRETS_PLUGIN_MASTER_KEY` (64 hex chars or 44 base64 chars; generate with
`openssl rand -hex 32`). The operator holds it in their secret store and
renders it into the plugin's `env:` — the plugin never creates an
unencrypted vault and refuses to start without a valid key. Losing the key
loses the vault: there is no backdoor.

Per-caller isolation means each caller's vault is encrypted with the *same*
master key but lives in its own file; a compromised plugin can only decrypt
its own file.

## Actions

| Action | Params | Result |
|---|---|---|
| `secret_set` | `{name, value}` | `{stored: true}` |
| `secret_get` | `{name}` | `{found, value}` |
| `secret_delete` | `{name}` | `{deleted}` |
| `secret_list` | `{}` | `{names: [...]}` (sorted; names are not secret, values are) |

Secret names are `[a-zA-Z0-9_.-]`, max 256 bytes. Values are max 256 KiB.
Size caps reject — they never truncate.

## Provisioning another plugin's vault

`secret_set` writes the *caller's* vault, so an operator calling it (from
`vyn-act`, the agent, a terminal client) fills their own vault — never the
one `email`, `ai`, `search`, `stt` or `mqtt` reads with `secret_get`.
`secrets-provision` writes a plugin's vault file directly, with the same key
and format:

```sh
# same env the plugin runs with (plugins.d/secrets.yaml)
export SECRETS_PLUGIN_DATA_DIR=... SECRETS_PLUGIN_MASTER_KEY=...
printf %s "$PASSWORD" | secrets-provision mqtt MQTT_PLUGIN_PASSWORD
vyn plugin -c ~/.config/vyn/config.yaml --token "$(cat ~/.config/vyn/admin_token)" restart secrets
```

The value comes from stdin (one trailing newline dropped), never argv.
Restart `secrets` afterwards: a vault the plugin already loaded stays cached,
and its next write for that caller would drop the provisioned value.

## Config

Config is read from the environment (the kernel's plugin supervisor
translates the `config.yaml` `env:` entry into these before spawning the
plugin). Copy `config.example.yaml` for the documented defaults.

| Env var | Default | Meaning |
|---|---|---|
| `SECRETS_PLUGIN_DATA_DIR` | — (required) | directory holding per-caller `.vault` files |
| `SECRETS_PLUGIN_MASTER_KEY` | — (required) | 32-byte master key, 64 hex / 44 base64 chars |
| `SECRETS_PLUGIN_MAX_NAME_BYTES` | `256` | secret name length cap |
| `SECRETS_PLUGIN_MAX_VALUE_BYTES` | `262144` (256 KiB) | secret value size cap |

## Concurrency

Storage-class plugin on the hot path, so it drives the SDK's concurrent
message loop (`ConcurrentHandler` + `serve_concurrent`, `vynkor-sdk` ≥ 0.1.4)
exactly like `database`/`network`. Each caller's decrypted vault is cached
behind a per-caller lock; every mutation re-encrypts and atomically persists
that caller's file while holding its lock.

## Status

v1. `PERMISSION_SECRETS` is defined in the wire protocol (v1.4+, value 15)
and already accepted by the kernel's `known_permissions()` probe; with the
Manifest v2 per-action `permission: "secrets"`, the kernel enforces the gate
data-driven (both caller and provider must hold `PERMISSION_SECRETS`) with
no kernel change. The manifest declares the published requirements
(`vynkor-sdk = "0.1"`, `vynkor-wire = "0.2"`), which resolve from crates.io.
