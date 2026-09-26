# PLAY-04: Voice DJ — "play something for work"

`agent` picks → `media` (MPRIS players) or `library` + `sound` (local files).

## Via the agent (recommended)

```json
{
  "action": "goal_start",
  "params": {"goal": "Play something for focused work. Prefer my local library: library_search for lofi/jazz/ambient audio, pick one track and sound_play it. If nothing is found, media_play on the active player."}
}
```

Allowlist needed in `AGENT_PLUGIN_ALLOWED_ACTIONS`: `library_search`,
`library_random`, `sound_play`, `media_play`, `media_shuffle`.

## Deterministic variant (no LLM)

```json
{"action": "library_search", "params": {"query": "lofi", "kind": "audio", "limit": 5}}
// → {"results": [{"path": "/music/lofi/01.mp3", ...}], "total": 1}
{"action": "sound_play", "params": {"file": "/music/lofi/01.mp3"}}
```

Random pick:

```json
{"action": "library_random", "params": {"kind": "audio", "count": 1}}
```

MPRIS player (Spotify, mpd, browser):

```json
{"action": "media_play", "params": {"player": "spotify"}}
{"action": "media_shuffle", "params": {"enabled": true, "player": "spotify"}}
{"action": "media_loop", "params": {"mode": "playlist", "player": "spotify"}}
```

## Via `vyn ask`

```bash
vyn ask "play something for work"
vyn ask "play jazz for focus"
```

## Notes

- `media` controls other players, `sound` owns the speakers for local files.
- `sound` plays one clip at a time (no queue) — for a playlist use an MPRIS
  player; a `sound` queue is on `plugins/sound/ROADMAP.md` "Later".
- `library` must have scanned the roots first: `library_scan {}`
  (`LIBRARY_PLUGIN_ALLOWED_ROOTS`).
