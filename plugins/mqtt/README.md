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
| `mqtt_device_command` | Send command to device |
| `mqtt_device_telemetry` | Get device telemetry |
| `mqtt_ota_update` | Start OTA update |

## Configuration

No environment configuration yet — the broker, credentials and TLS files
are passed per call to `mqtt_connect`:

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

Only `broker_host` is required. The connection lives in the plugin
process: after a plugin restart, call `mqtt_connect` again.

> The password travels as an action parameter, so it is visible to whoever
> builds the call (e.g. an agent transcript). Env defaults, vault-first
> credentials via `secrets`, and auto-connect at startup are planned — see
> `ROADMAP.md`.

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
**Actions:** 11 registered  
**Streaming:** Supported via R6

## See Also

- [Protocol Specification](../../docs/mqtt/PROTOCOL.md)
- [Implementation Plan](../../docs/mqtt/PLAN.md)
- [ESP Library](../../libraries/vynkor-esp/) (coming soon)
