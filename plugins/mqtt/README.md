# MQTT Plugin

MQTT connectivity for Vynkor — talk to ESP devices, Zigbee gateways, and other IoT hardware.

## Overview

The MQTT plugin provides a bridge between Vynkor and IoT devices using the [Vynkor Device Protocol (VDP)](../../docs/mqtt/PROTOCOL.md). It supports:

- **WiFi devices** via MQTT (ESP8266/ESP32, Tasmota, ESPHome, custom firmware)
- **Zigbee devices** via MQTT bridge (Zigbee2MQTT)
- **Thread devices** via MQTT bridge (OpenThread Border Router)

## Actions

### Connection

| Action | Description |
|---|---|
| `mqtt_connect` | Connect to MQTT broker |
| `mqtt_disconnect` | Disconnect from broker |
| `mqtt_status` | Get connection status |

### Messaging

| Action | Description |
|---|---|
| `mqtt_publish` | Publish message to topic |
| `mqtt_subscribe` | Subscribe to topic (R6 streaming) |
| `mqtt_unsubscribe` | Unsubscribe from topic |

### Devices

| Action | Description |
|---|---|
| `mqtt_device_list` | List discovered devices |
| `mqtt_device_info` | Get device details |
| `mqtt_device_command` | Send an allowlisted command to device (no confirmation) |
| `mqtt_device_command_confirmed` | Send any command to device (asks the user first) |
| `mqtt_device_telemetry` | Get device telemetry |
| `mqtt_ota_update` | Start OTA update |

## Configuration

Auto-connect at startup (all optional; without `MQTT_PLUGIN_BROKER_HOST`
the plugin waits for an explicit `mqtt_connect`):

| Env | Meaning |
|---|---|
| `MQTT_PLUGIN_BROKER_HOST` | Broker host |
| `MQTT_PLUGIN_BROKER_PORT` | Broker port (default 1883) |
| `MQTT_PLUGIN_USERNAME` | Broker username |
| `MQTT_PLUGIN_PASSWORD` | Broker password — only as a fallback, see below |
| `MQTT_PLUGIN_COMMAND_ALLOWLIST` | Commands `mqtt_device_command` may run unconfirmed |

`mqtt_connect` takes the same settings per call, plus TLS files:

```json
{
  "broker_host": "mqtt.local",
  "broker_port": 1883,
  "client_id": "vynkor-mqtt-plugin",
  "username": "user",
  "password": "pass",
  "keepalive_secs": 60,
  "tls": {"ca_cert_path": "/path/to/ca.pem", "client_cert_path": "...", "client_key_path": "..."}
}
```

Wrong credentials fail the call. An unreachable broker does not: the call
returns `connected: false` with `last_error`, and the plugin keeps retrying
(also after a broker restart, re-subscribing on reconnect).

**Password.** Auto-connect resolves it vault-first: `secret_get
MQTT_PLUGIN_PASSWORD` from the `secrets` plugin (retried for ~5 s in case
`secrets` registers later), then the env var of the same name. Keep it out
of `plugins.d/mqtt.yaml`:

```sh
printf %s "$PW" | secrets-provision mqtt MQTT_PLUGIN_PASSWORD  # see plugins/secrets
vyn ... plugin restart secrets                                 # drop its cached vault
```

The `password` parameter of `mqtt_connect` is used as given: whoever builds
that call sees it, so prefer auto-connect.

### Command allowlist

The agent's confirmation gate is fixed per action, so device control is
split in two:

- `mqtt_device_command` (risk low, no confirmation) runs only what
  `MQTT_PLUGIN_COMMAND_ALLOWLIST` admits and refuses the rest with
  `not allowed`, before anything is published;
- `mqtt_device_command_confirmed` (risk high) runs anything, after the
  user confirms. `mqtt_publish` is gated the same way, since it can reach
  `vynkor/*/command` directly.

Rules are `device:command[:target]`, comma-separated. `*` matches any
device or command; `target` pins a rule to one pin, matched against
`params.name` or `params.pin` (a call setting both, or a non-scalar
value, never matches a target rule). Default: `*:get_state,*:ping` (read-only).

```
MQTT_PLUGIN_COMMAND_ALLOWLIST=uesp12e-001:set_pin:led_d1,uesp12e-001:toggle_pin:led_d1,*:get_state,*:ping
```

## Quick Start

### 1. Connect to broker

```json
// mqtt_connect
{
  "broker_host": "mqtt.local",
  "broker_port": 1883,
  "client_id": "vynkor-plugin"
}
```

### 2. Subscribe to device telemetry

```json
// mqtt_subscribe
{
  "topic": "vynkor/+/telemetry",
  "qos": 1,
  "stream": true
}
```

### 3. Send command to device

```json
// mqtt_device_command
{
  "device_id": "esp-kitchen",
  "command": "set_relay",
  "params": {"pin": 5, "state": true}
}
```

## Protocol

Uses [Vynkor Device Protocol (VDP)](../../docs/mqtt/PROTOCOL.md) — a protobuf-based protocol for IoT communication.

**Topic structure:**
```
vynkor/{device_id}/telemetry    — sensor data
vynkor/{device_id}/command      — commands to device
vynkor/{device_id}/response     — device responses
vynkor/{device_id}/state        — device state (retained)
vynkor/{device_id}/status       — online/offline (LWT)
```

## Permissions

Requires `PERMISSION_NETWORK` (for MQTT connection) and optionally `PERMISSION_SECRETS` (for password vault).

## Status

**Stage:** MVP (Phase 1)  
**Actions:** 12 registered  
**Streaming:** Supported via R6

## See Also

- [Protocol Specification](../../docs/mqtt/PROTOCOL.md)
- [Implementation Plan](../../docs/mqtt/PLAN.md)
- [ESP Library](../../libraries/vynkor-esp/) (coming soon)
