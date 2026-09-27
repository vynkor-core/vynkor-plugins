# weather plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Shipped

- 0.1.0 — `weather_now`, `weather_forecast` (Open-Meteo, no key).
- 0.1.1 — home location + timezone/timeout defaults from env (the manifest
  advertised them before the code read them).

## Next

- **Place names** — `place: "Tashkent"` resolved through Open-Meteo's
  geocoding API (also keyless), so neither the user nor the agent needs
  coordinates.
- **Hourly forecast** — `weather_hourly {hours}` for "will it rain at 6?".
- **Human summary** — `summary` string ("light rain, 12…18 °C") from
  `weather_code`, so a TTS briefing doesn't need an LLM step.
- **Alerts** — optional poll publishing `plugin.weather.alert` (rain within
  N hours, frost) for `automations`.
- **Units** — `units: metric|imperial`.
