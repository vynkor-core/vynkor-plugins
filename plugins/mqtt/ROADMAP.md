# mqtt plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next

- **Env config + auto-connect** — `MQTT_PLUGIN_BROKER`, `_USERNAME`,
  `_CA_CERT_PATH`, `_CLIENT_CERT_PATH`, `_CLIENT_KEY_PATH`, `_TOPIC_PREFIX`,
  and connect at startup. Today every restart needs an explicit
  `mqtt_connect` with the broker in the params. (These names were once
  advertised in `config_schema` without being implemented; they were
  removed from the schema on 2026-09-27 — add them back with the code.)
- **Vault-first password** — `password_env` resolved via `secrets`
  (`PERMISSION_SECRETS`), instead of a plaintext password in action params
  that ends up in agent transcripts.
- **Reconnect** — keep the session alive across broker restarts with
  backoff; resubscribe.
- **Persisted device registry** — survive plugin restarts (`database`).
- **Events** — telemetry/state changes as `plugin.mqtt.*` events so
  `automations` can react (INT-09's Zigbee2MQTT/Tasmota use cases).
- **Least privilege** — the manifest declares `network` and `secrets`, but
  the plugin opens its own broker socket (rumqttc, not `network`'s
  `http_request`) and never calls `secret_get`. Drop both until the
  vault-first password lands (then keep only `secrets`); like `email`, it
  needs `sandbox: false` for the raw socket.
