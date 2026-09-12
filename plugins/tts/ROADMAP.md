# tts plugin roadmap

Goal: give any Vynkor plugin a way to synthesize speech — one blessed
path, provider quirks/auth/voice handling in one place instead of every
plugin rolling its own client. Local-first: a fully offline engine is the
default, cloud providers are opt-in additions behind the same interface.

## Decision: local in-process, cloud via `network`

Two halves, two different mechanics:

- **Cloud providers (`openai`, `elevenlabs`)** do **not** open their own
  sockets and declare no `PERMISSION_NETWORK`. They call the
  kernel-routed `http_request` action (owned by the `network` plugin) via
  `VynkorClient::send_action` — identical to `ai`. SSRF blocklist,
  redirect handling, retry-backoff and response size caps in `network`
  apply for free; `tts`'s `plugin.json` has `"permissions": []`.
- **Local provider (`sherpa`)** opens no sockets at all. It links
  sherpa-onnx, loads an ONNX model from disk, and synthesizes in-process.
  This is the deliberate shape of "local": no daemon, no subprocess, no
  network hop — the audio never leaves the machine. It's also why the
  plugin ships as a self-contained binary rather than shelling out to a
  `piper`/`kokoro` CLI.

Two-tier design keeps the attack surface honest: cloud key material lives
behind the same `TTS_PLUGIN_ALLOWED_KEY_ENVS` allowlist `ai` uses, and the
local model path is operator-set (`TTS_PLUGIN_LOCAL_MODEL_DIR`), never
caller-controlled — a caller-supplied model path would be an arbitrary
file-read primitive.

## Naming

Plugin id: `tts`. Binary: `tts`. Mirrors `ai`/`network` — short, matches
the "one blessed path per capability" convention.

## v1 scope

- `tts_synthesize` action:

  Request (`ActionRequest.params_json`):
  ```json
  {
    "provider": "sherpa",
    "text": "Hello from Vynkor.",
    "voice": "af_heart",
    "format": "wav",
    "speed": 1.0
  }
  ```
  - `provider`: `sherpa` (local) | `openai` | `elevenlabs`.
  - `api_key_env` (cloud only): env var name the `tts` process reads at
    call time, allowlisted via `TTS_PLUGIN_ALLOWED_KEY_ENVS`. Caller never
    puts the raw key in the payload.
  - Local voices: Kokoro names from the official 26-voice table
    (`af_heart`, ...) mapped to sherpa sids, with `sid:N` as an escape
    hatch; piper maps any name to sid 0 (single-speaker models).

  Response (`ActionResponse.data_json`) on success, normalized:
  ```json
  {
    "format": "wav",
    "sample_rate_hz": 24000,
    "num_channels": 1,
    "duration_seconds": 2.4,
    "audio_base64": "UklGR..."
  }
  ```

- `tts_voices` action: list voices. Real data for `sherpa` (from the
  loaded model), static list for `openai`, rejected for `elevenlabs`
  (per-account).

- Errors → `ACTION_ERROR` with human-readable messages; never leak a
  resolved key. Local model failures surface the missing file / load
  failure instead of a generic "provider unavailable".

## v1 implementation design (confirmed 2026-08-10)

**Crate layout** (mirrors `ai`):

```
plugins/tts/
  Cargo.toml          # bin `tts`, lib `tts_plugin`
  plugin.json          # permissions: [], actions: ["tts_synthesize","tts_voices"]
  src/
    main.rs             # custom serve loop (same rationale as ai: the SDK
                        # Plugin::run can't hand out a VynkorClient)
    request.rs           # parse_request: validate SynthesizeParams
    provider/
      mod.rs               # Provider trait (cloud), AudioResult, VoiceInfo,
                           # WAV encode/decode helpers, Kokoro voice table
      sherpa.rs            # local engine: sherpa-onnx OfflineTts, lazy load,
                           # kokoro + piper model families
      openai.rs            # POST /v1/audio/speech (Bearer)
      elevenlabs.rs        # POST /v1/text-to-speech/{voice_id} (xi-api-key)
    handler.rs            # parse -> sherpa in-process OR network http_request -> normalize
```

**Local engine details.** Model loaded lazily on first `sherpa` request,
cached in a process-lifetime `OnceLock` (failed loads cached too — no
error-flood retry). `TTS_PLUGIN_LOCAL_MODEL_TYPE` selects `kokoro`
(`model.onnx` + `voices.bin` + `tokens.txt` + `espeak-ng-data/`, optional
auto-detected `lexicon-*.txt`/`dict/` for multilingual) or `piper`
(`model.onnx` + `tokens.txt` + `espeak-ng-data/`). Output is always 16-bit
PCM; `wav` wraps it in a header, `pcm` returns it raw. `format` for
`sherpa` is `wav`/`pcm` only (no mp3 encode in v1).

**Cloud adapters.** `openai`: `POST {base_url}/audio/speech` with
`response_format` mapped from the caller's `format` (pcm → raw 24 kHz
mono, wav → header-parsed metadata, mp3 → passthrough, metadata 0).
`elevenlabs`: `POST {base_url}/v1/text-to-speech/{voice}` with
`output_format=mp3_44100_128` or `pcm_24000`. Both clamp their HTTP
timeout to `network`'s 30 s cap.

**Testing.** 50 unit tests, no live network, no model files: request
validation, allowlist, WAV encode/decode round-trips, Kokoro sid
resolution, piper sid rules, cloud request building and fixture-response
parsing. sherpa config assembly tested without loading a real model.

## Known bugs (live-kernel audit 2026-08-22)

> **Fixed 2026-08 (`fix/live-audit-defects`, merged):** all blocking sherpa
> calls now run via `tokio::task::spawn_blocking` so ONNX init/load can't
> stall the async serve loop; isolated probes with real models return
> `voices` instantly. See `docs/LIVE_KERNEL_AUDIT_2026-08-22.md` defect #2.

- **Local `sherpa` actions hang indefinitely before the model loads.**
  First full live-kernel audit (`docs/LIVE_KERNEL_AUDIT_2026-08-22.md`,
  defect #2): `tts_voices`/`tts_synthesize` with provider `sherpa`
  (piper ru medium on disk, path + type set via env) never respond —
  plugin process sleeps with 0% CPU and RSS flat at ~22 MB, i.e. sherpa
  init never starts. Kernel deadline fires `ACTION_TIMEOUT` ~200 s later.
  100% repro in that environment; earlier manual Kokoro E2E (see README
  "Testing") did work, so this is either a regression since then or an
  environment-specific blocker (e.g. something in the supervised process
  env sherpa-onnx init waits on). Fix direction: reproduce outside the
  kernel by invoking the handler directly with identical env; if it only
  hangs supervised, diff the supervisor env/spawn shape vs a bare run;
  add an integration test that runs one real synthesize against a model
  file so this can't silently rot again.

## Near-term (buildable now, no kernel changes)

- **Google Cloud TTS + Azure adapters** — same `Provider` trait, one file
  each; GCP needs its own auth path (bearer from a service account), so
  deferred past the two key-based clouds.
- **More output formats** — ✅ done (2026-08-21): `opus`/`aac`/`flac` for
  `openai`; `ulaw` (→ `ulaw_8000`) for `elevenlabs`; a `mp3` encode path
  for local output (`mp3lame-encoder` crate → LAME) so `sherpa` can serve
  MP3 too.
- **Streaming synthesis** — sherpa-onnx supports generation callbacks
  (per-sentence audio as it's produced); today's model is one
  `ActionRequest` → one `ActionResponse`, so v1 buffers the full clip.
  Revisit once `ActionStreamChunk` (R6-02) lands in the kernel.
  (2026-08-15: D-12 shipped `tts_speak` — synthesize → PCM → Opus →
  `AudioStreamChunk` stream to a peer — as the host→client speaker leg,
  still one shot of the full clip rather than per-sentence callbacks.
  2026-08-26: EXI-02 shipped `tts_speak_stream` — sentence-level
  streaming, first audio after one phrase — **shipped (v0.1.1)**.)
- **Model hot-reload** — re-read `TTS_PLUGIN_LOCAL_MODEL_*` on an
  operator-triggered action instead of requiring a process restart.

## Requires kernel/protocol changes

- **Streaming action support (R6-02)** — real audio streaming to the
  caller instead of buffer-then-reply. Same `ActionStreamChunk` blocker
  as `ai`/`network` — see `vynkor/ROADMAP.md` R6-02.
- **`tts.synthesis_done` events (R6-01)** — publish duration/format/voice
  to the event bus for observability, same plugin → event-bus publish
  path blocker as `ai` (R6-01).
- **Per-caller quotas (R6-03)** — local synthesis is CPU-bound; if `tts`
  becomes a shared resource, same missing `caller-id` open question as
  `network`/`ai`.

## Non-goals / follow-ups

- No kernel special-casing for "TTS" — an ordinary plugin like any other.
- No audio playback / speaker output in the plugin — callers get bytes and
  decide (file, player, stream). Playback is a caller concern.
- No voice cloning / fine-tuning — provider features only.
- No SSML in v1 — plain text; SSML passthrough is a per-provider adapter
  change once a caller needs it.
