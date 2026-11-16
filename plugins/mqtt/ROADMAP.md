# mqtt plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Done (2026-10-26)

- Env auto-connect (`MQTT_PLUGIN_BROKER_HOST`, `_PORT`, `_USERNAME`,
  `_PASSWORD`); reconnect with resubscribe; background message consumer
  (registry no longer waits for an action); real disconnect; command
  allowlist with a confirmed fallback action.
- Reference firmware `firmware/vynkor-gpio` 0.3.0 (MicroPython, ESP8266):
  input/ADC pins, DHT11/22 and DS18B20 drivers set by `config_set`,
  flash-pin guard, hardware + connectivity watchdog, no lost commands,
  half-open link detection. Running on `uesp12e-001`.
- Vault-first broker password for auto-connect (`secret_get
  MQTT_PLUGIN_PASSWORD`, env fallback), provisioned with `secrets-provision`.

## Next

- **Manifest/code gaps** — TLS fields are top-level in the schema but read
  from `params.tls`; mTLS client cert/key are parsed and ignored;
  `mqtt_device_telemetry` ignores `stream`/`since`/`limit`; `encoding:
  protobuf` is advertised but rejected; `mqtt_subscribe stream=true` only
  traces.
- **Staleness** — mark a device offline when `last_seen` is older than
  ~2x its reporting interval, not only on LWT.
- **Vault-first password for `mqtt_connect`** — auto-connect is done; the
  action still takes a plaintext `password`. Needs a `password_secret` name
  param, and a kernel client inside `on_message` to call `secret_get`.
- **Persisted device registry** — survive plugin restarts (`database`).
- **Events** — telemetry/state changes as `plugin.mqtt.*` events so
  `automations` can react (INT-09's Zigbee2MQTT/Tasmota use cases).
- **Firmware** — OTA for `.py`/`.mpy` with rollback (`mqtt_ota_update`
  is declared, firmware answers "not supported"); provisioning AP instead
  of Wi-Fi credentials in `config.json`; only ~17 KB RAM left on ESP8266,
  so freeze modules into a custom build before adding drivers.
- **Least privilege** — the manifest declares `network`, but the plugin
  opens its own broker socket (rumqttc, not `network`'s `http_request`).
  Drop it and keep `secrets` (used by auto-connect); like `email`, it needs
  `sandbox: false` for the raw socket.
