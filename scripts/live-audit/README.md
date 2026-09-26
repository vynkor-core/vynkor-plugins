# live-audit harness

Snapshot of the working harness used for the 2026-08-22 live-kernel audit
(`docs/LIVE_KERNEL_AUDIT_2026-08-22.md`). Kept as-is so the next audit has
a starting point; paths below are hardcoded to a scratch dir and will need
adjusting before reuse.

| File | Role |
|---|---|
| `vynkor_ws.py` | Minimal external WS client: 44-byte frame codec, protobuf envelopes, HKDF session-MAC derivation, `call()` that routes via target=`kernel` and matches `ActionResponse`/`error` envelopes |
| `gen_dropins.py` | Generates `plugins.d/*.yaml` drop-ins (binary path, per-plugin env, minted `VYN_JWT_TOKEN` with `sub == plugin_id`; the kernel injects the frame-MAC key) |
| `simple_test.py` | Functional matrix: representative actions per plugin with expected-result notes |
| `load_test.py` | Burst probe (125 db ops + 5 HTTPS GET) with a /proc RSS+CPU sampler around it |

Run shape: generate drop-ins → start kernel with a test `config.yaml`
(TLS on 8130, jwt_secret ≥ 32 bytes) → wait for registration
(`GET /plugins`) → `python -u simple_test.py`, then `load_test.py`.
Requires Python ≥3.10 with `websockets`, `protobuf`, `cryptography`,
`zstandard`; imports generated types from `../vynkor-sdk-python`.

Gotchas encoded here, learned the hard way:

- JWT `sub` must equal the registering `plugin_id`, and token claims
  override manifest permissions — mint per plugin.
- `ipc_targets` is exact-match; no wildcard.
- Action requests go to frame target **`kernel`**, never to the plugin slug.
- `params_json`/`data_json` are protobuf `bytes`, not strings.
