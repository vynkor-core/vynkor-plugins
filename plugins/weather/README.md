# weather plugin

Open-Meteo current + daily forecast for vynkor — fully outbound, no API key.

Routes every call through `network`'s gated `http_request` (declares `PERMISSION_NETWORK`, T-19). No `secrets`, no vault, no `base_url` allowlist — `open-meteo.com` is public.

## Actions

| Action | Params | Result |
|---|---|---|
| `weather_now` | `{lat?, lon?, timezone?, timeout_ms?}` | `{latitude, longitude, timezone, current, raw}` |
| `weather_forecast` | `{lat?, lon?, days?(1..16), timezone?, timeout_ms?}` | `{latitude, longitude, timezone, daily, raw}` |

- `lat` `-90..90`, `lon` `-180..180`, finite. Give both, or omit both to use
  the home location from `WEATHER_PLUGIN_DEFAULT_LAT`/`_LON` — one without
  the other is rejected rather than mixed with the config.
- `timezone` IANA e.g. `Europe/Berlin` or `auto` (default
  `WEATHER_PLUGIN_DEFAULT_TIMEZONE`, else `auto`).
- `days` 1..16, default 3.
- `timeout_ms` 1..30000, default `WEATHER_PLUGIN_TIMEOUT_MS` (10000).

Open-Meteo endpoints:
- `current`: `https://api.open-meteo.com/v1/forecast?...&current=temperature_2m,...&timezone=...`
- `forecast`: `...&daily=temperature_2m_max,...&forecast_days=N&timezone=...`

`raw` is the full provider payload for debugging / extra fields (hourly `current_units`, `daily_units`, `elevation`, etc.). Agent can pick extra fields without a new plugin version.

## Examples

```json
// now
{ "lat": 55.75, "lon": 37.61, "timezone": "Europe/Moscow" }
→ { "current": {"temperature_2m": 18.2, "wind_speed_10m": 3.4, "weather_code": 1}, "raw": {...} }

// forecast 7 days
{ "lat": 55.75, "lon": 37.61, "days": 7 }
→ { "daily": {"time":["2026-08-26", ...], "temperature_2m_max":[22, ...]}, "raw": {...} }
```

For `PLAY`-briefing: call `weather_forecast {lat,lon,days:1}` + `calendar.due` + optional `rss` → `notify` or `tts_synthesize`.

## Config

| Env | Default | Meaning |
|---|---|---|
| `WEATHER_PLUGIN_DEFAULT_LAT` | — | Home latitude used when a call omits both `lat` and `lon` |
| `WEATHER_PLUGIN_DEFAULT_LON` | — | Home longitude (set both, or neither) |
| `WEATHER_PLUGIN_DEFAULT_TIMEZONE` | `auto` | Timezone when a call omits `timezone` |
| `WEATHER_PLUGIN_TIMEOUT_MS` | `10000` | Default HTTP timeout per open-meteo call (capped at 30000) |

With a home location set, `weather_now {}` answers "what's the weather?"
without the agent having to know where you are.

## Testing

`cargo test` — unit tests for `lat/lon` validation, e2e against fake kernel (UnixStream::pair handshake, fake `network` returning fixture JSON, asserts normalized output and that `http_request` hit `api.open-meteo.com`). No live network.
