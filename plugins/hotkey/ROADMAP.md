# hotkey plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next

- **`PERMISSION_HOTKEY`** (enum 24) — lands in the same wire bump as
  wifi/bluetooth/input/camera (20–23; installer-probe gap rule). Until then
  the plugin holds `PERMISSION_SYSTEM` + `PERMISSION_EVENT_PUBLISH`.
- **X11 backend** — `XGrabKey` for X11 sessions (today: portal on Wayland,
  `hotkey_inject` / compositor binds elsewhere).
- **Chords & double-tap** — `Super+K, Super+T` sequences and double-tap
  triggers for more bindings without more modifiers.
- **Binding persistence check** — surface in `hotkey_status` when the portal
  silently dropped a binding after a compositor restart.
