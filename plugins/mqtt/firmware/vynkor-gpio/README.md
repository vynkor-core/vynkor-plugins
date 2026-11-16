# vynkor-gpio firmware

MicroPython firmware that speaks VDP over MQTT (`vynkor/{id}/command`,
`/response`, `/status`, `/telemetry`). Tested on ESP8266 (ESP-12E,
MicroPython 1.27); written to run on ESP32 as well (untested).

## Pins and sensors

Added at runtime with `config_set` and kept in `overrides.json`; no reflash:

```json
{"pins": {"btn": {"pin": 4, "mode": "in", "pull": "up", "active_low": true},
          "light": {"pin": 0, "mode": "adc"},
          "old": null},
 "sensors": {"room": {"driver": "dht22", "pin": 5}}}
```

| mode | | |
|---|---|---|
| `out` | digital output | `set_pin`, `toggle_pin` |
| `pwm` | duty 0..1023 | `set_pwm` |
| `in` | debounced input (`debounce_ms`, default 30) | change → telemetry stream `input` |
| `adc` | `read_u16` (ESP8266: pin 0 = A0 only) | `get_pin` |

Drivers: `dht11`, `dht22`, `ds18b20` (read without blocking the loop;
telemetry stream `sensors`, command `read_sensor`). GPIO 6–11 (flash) are
refused, and a GPIO can be claimed once.

## Watchdogs

- Hardware: `machine.WDT`, ~2.2 s on ESP8266, cannot be stopped. Every wait
  feeds it (`Agent.nap`, the CONNACK loop); TCP connect times out at 1.5 s.
  `mqtt.host` must be an IP: `getaddrinfo` blocks without feeding it.
- Connectivity: no MQTT for `offline_reset_s` (default 600) → reboot; every
  3rd failed Wi-Fi join restarts the radio.

**Maintenance:** `boot.py` waits 3 s before starting the app. Ctrl-C in that
window gives a REPL with the watchdog off. Opening the serial port resets
the board, so keep sending Ctrl-C right after opening it (plain `mpremote`
is too early and fails with "could not enter raw repl").

## Build, test, upload

```sh
python3 -m unittest discover -s tests          # host tests, stubs in stubs/
mpy-cross -march=xtensa -o app.mpy app.py      # mpy-cross 1.27.x
mpy-cross -march=xtensa -o mqtt.mpy mqtt.py
# upload app.mpy, mqtt.mpy, boot.py; config.json (Wi-Fi/MQTT secrets) lives
# only on the device, see config.example.json
```
