# tts plugin — caller's guide

Everything a plugin (or the kernel) needs to speak to the `tts` plugin.
Actions: `tts_synthesize`, `tts_voices`.

## `tts_synthesize`

Turn text into audio bytes.

### Request

```json
{
  "provider": "sherpa",
  "text": "Hello from Vynkor.",
  "voice": "af_heart",
  "format": "wav",
  "speed": 1.0,
  "timeout_ms": 60000
}
```

| Field | Required | Meaning |
|---|---|---|
| `provider` | yes | `sherpa` (local) \| `openai` \| `elevenlabs` (cloud) |
| `text` | yes | 1–4000 chars; trimmed |
| `voice` | yes | provider-specific id (below) |
| `api_key_env` | cloud only | lookup handle (env-var-style name) for the provider key — `tts` reads it from the `secrets` plugin's vault first, then falls back to its own env; must be on the operator's `TTS_PLUGIN_ALLOWED_KEY_ENVS` allowlist. Never pass a literal key. |
| `format` | no | `sherpa`: `wav` (default) \| `pcm` \| `mp3`. `openai`: `mp3` (default) \| `wav` \| `pcm` \| `opus` \| `aac` \| `flac`. `elevenlabs`: `mp3` (default) \| `pcm` \| `ulaw` |
| `speed` | no | `0.25`–`4.0`, default `1.0`, clamped |
| `timeout_ms` | no | default 30000, capped at 60000; cloud hops additionally capped at 30000 by `network` |
| `base_url` | no | override the provider endpoint |
| `model` | no | override the provider model |

### Response

```json
{
  "format": "wav",
  "sample_rate_hz": 24000,
  "num_channels": 1,
  "duration_seconds": 2.4,
  "audio_base64": "UklGR..."
}
```

- `audio_base64` — standard base64 of the audio bytes; decode and write to
  a file (`.wav` / `.mp3` / `.pcm` / `.opus` / `.aac` / `.flac` / `.ulaw`
  per `format`) or pipe to a player.
- `sample_rate_hz` / `num_channels` — real for `wav`/`pcm` and for
  sherpa-local `mp3` (the source rate is known); `0` for `mp3`/`opus`/
  `aac`/`flac` from a cloud provider (the container carries no header we
  trust). `ulaw` reports `sample_rate_hz` 8000 (its fixed rate).
- `duration_seconds` — real for `wav`/`pcm` and sherpa-local `mp3`; `0`
  for cloud `mp3`/`opus`/`aac`/`flac`.

### Voices per provider

- **sherpa / kokoro** — names from the official table: `af_heart`,
  `af_bella`, `af_nicole`, `af_aoede`, `af_kore`, `af_sarah`, `af_nova`,
  `af_sky`, `am_adam`, `am_echo`, `am_eric`, `am_fenrir`, `am_liam`,
  `am_michael`, `am_onyx`, `am_puck`, `am_santa`, `bf_alice`, `bf_emma`,
  `bf_isabella`, `bf_lily`, `bm_daniel`, `bm_fable`, `bm_george`,
  `bm_lewis`, `ff_siwis`. Escape hatch for custom voice files: `sid:N`.
  Ask the plugin: `tts_voices` with `{"provider":"sherpa"}`.
- **sherpa / piper** — single-speaker: any non-empty `voice` works (maps
  to sid 0). Multi-speaker models: `sid:N`.
- **openai** — `alloy`, `ash`, `ballad`, `coral`, `echo`, `fable`,
  `onyx`, `nova`, `sage`, `shimmer`, `verse`, `amethyst` (validated at
  parse time; unknown → error naming the list).
- **elevenlabs** — any voice id from your account (`21m00Tcm4TlvDq8ikWAM`
  is the classic "Rachel"): list via the ElevenLabs dashboard or
  `GET /v1/voices`.

### Examples

Local Kokoro, WAV out:

```json
{
  "provider": "sherpa",
  "text": "The quick brown fox jumps over the lazy dog.",
  "voice": "af_heart"
}
```

Local Piper, raw PCM:

```json
{
  "provider": "sherpa",
  "text": "Offline, private, fast.",
  "voice": "anything",
  "format": "pcm"
}
```

OpenAI, MP3 out:

```json
{
  "provider": "openai",
  "text": "Hello from the cloud.",
  "voice": "nova",
  "api_key_env": "OPENAI_API_KEY",
  "format": "mp3",
  "model": "gpt-4o-mini-tts"
}
```

ElevenLabs, PCM at 24 kHz:

```json
{
  "provider": "elevenlabs",
  "text": "Hello from ElevenLabs.",
  "voice": "21m00Tcm4TlvDq8ikWAM",
  "api_key_env": "ELEVENLABS_API_KEY",
  "format": "pcm"
}
```

Local Kokoro, MP3 out (PCM → LAME encode in-process):

```json
{
  "provider": "sherpa",
  "text": "Offline and compact.",
  "voice": "af_heart",
  "format": "mp3"
}
```

ElevenLabs, μ-law at 8 kHz (telephony/Twilio):

```json
{
  "provider": "elevenlabs",
  "text": "Hello from the phone network.",
  "voice": "21m00Tcm4TlvDq8ikWAM",
  "api_key_env": "ELEVENLABS_API_KEY",
  "format": "ulaw"
}
```

OpenAI, Opus out (low-latency streaming):

```json
{
  "provider": "openai",
  "text": "Hello from the cloud.",
  "voice": "nova",
  "api_key_env": "OPENAI_API_KEY",
  "format": "opus"
}
```

OpenAI-compatible / self-hosted endpoint (any server speaking
`POST /v1/audio/speech`):

```json
{
  "provider": "openai",
  "text": "Local gateway.",
  "voice": "alloy",
  "api_key_env": "OPENAI_API_KEY",
  "base_url": "http://localhost:8880/v1"
}
```

(Pointing `base_url` at loopback requires `network`'s
`NETWORK_PLUGIN_ALLOWED_HOSTS=localhost,127.0.0.1` — see
`plugins/network/config.example.yaml`.)

## `tts_voices`

```json
{ "provider": "sherpa" }
```

Response:

```json
[
  { "id": "af_heart", "name": "af_heart" },
  { "id": "sid:26", "name": "speaker 26" }
]
```

`elevenlabs` → error (voices are per-account).

## `tts_speak`

D-12 voice pipeline: synthesize locally, encode the PCM as Opus, and stream
it as `AudioStreamChunk`s to a peer plugin (e.g. a client speaker) — the
host-TTS → client-speaker half of the pipeline. Local-only (`sherpa`): the
cloud providers return opaque mp3/ogg containers with no PCM source to
encode.

Request (`ActionRequest.params_json`):

```json
{
  "provider": "sherpa",
  "text": "Hello from the host.",
  "voice": "af_heart",
  "target": "phone-1.speaker",
  "stream_id": 1,
  "sample_rate_hz": 24000,
  "bitrate": 32000,
  "speed": 1.0
}
```

- `provider` — `"sherpa"` only. Required.
- `text` — required, 1..=4000 chars.
- `voice` — required; sherpa Kokoro name (`af_heart`, ...) or `sid:N`.
- `target` — required; the peer to stream to. For a remote device this is
  the mirrored capability id (`<device_id>.<cap>`, D-06); on one machine it can
  be any local plugin that receives `AudioStreamChunk`s.
- `stream_id` — optional, default `1`. Echoed in every chunk; lets the
  receiver demux concurrent streams.
- `sample_rate_hz` — optional; advertised stream rate. Defaults to the
  model's output rate; must be an Opus-supported rate
  (8000/12000/16000/24000/48000).
- `bitrate` — optional, default `32000`; `0` = codec default.
- `speed` — optional, `0.25`..=`4.0`, default `1.0`.

Response (`ActionResponse.data_json`) once the whole clip has been streamed:

```json
{
  "codec": "opus",
  "stream_id": 1,
  "target": "phone-1.speaker",
  "sample_rate_hz": 24000,
  "num_channels": 1,
  "duration_seconds": 2.4,
  "packets": 60
}
```

The audio itself is not in the response — it went out as a sequence of
`AudioStreamChunk` envelopes (codec `OPUS`, 20 ms frames, the last one with
`end_of_stream: true`) addressed to `target`, with each Opus packet in
`chunk.data`. Requires `PERMISSION_AUDIO_STREAM`.

The stream is fire-and-forget from the kernel's perspective: each chunk is
routed like any other message and there is no ack. A caller that needs
delivery guarantees should target a local plugin and handle absence of the
terminal `end_of_stream` chunk as a failure.

### `tts_speak_stream` — sentence-level streaming (EXI-02)

Same params and wire shape as [`tts_speak`](#action-tts_speak) (local
`sherpa` only), but the text is split into sentences first and each is
synthesized, Opus-encoded and streamed in turn — the peer starts hearing
audio after one phrase instead of after the whole paragraph. Only the final
packet of the final sentence carries `end_of_stream`. The response summary
adds `"sentences": N`. Use it for conversational paragraphs; keep
`tts_speak` for single short phrases (identical result, one less split).

## Errors

Every failure is `ACTION_ERROR` with a human-readable message in
`ActionResponse.error`. The resolved API key never appears in any error
string.

| Message (contains) | Cause |
|---|---|
| `invalid JSON: ...` | malformed request body |
| `missing required field: provider` | no `provider` |
| `unsupported provider: X` | unknown provider name |
| `missing required field: text` / `text must not be empty` / `text exceeds max length of 4000 chars` | bad `text` |
| `missing required field: voice` / `voice must not be empty` | bad `voice` |
| `unknown openai voice 'X' (known: ...)` | bad openai voice |
| `sherpa supports formats wav\|pcm\|mp3, got: X` | bad format for sherpa |
| `openai supports formats mp3\|wav\|pcm\|opus\|aac\|flac, got: X` | bad format for openai |
| `elevenlabs supports formats mp3\|pcm\|ulaw, got: X` | bad format for elevenlabs |
| `missing required field: api_key_env` / `api_key_env must not be empty` | cloud provider without a key reference |
| `api_key_env 'X' is not in the operator's TTS_PLUGIN_ALLOWED_KEY_ENVS allowlist` | key env not allowlisted |
| `key 'X' is neither in the secrets vault nor set as an environment variable` | allowlisted key handle absent from both the secrets vault and the env |
| `TTS_PLUGIN_LOCAL_MODEL_DIR is not set ...` | local provider, no model dir configured |
| `TTS_PLUGIN_LOCAL_MODEL_TYPE is not set ...` / `... is unsupported (use 'kokoro' or 'piper')` | local provider, bad model type |
| `missing required model file: <path>` | model dir lacks `model.onnx` / `voices.bin` / `tokens.txt` / `espeak-ng-data` |
| `sherpa-onnx failed to load model from ...` | model dir exists but the files don't form a loadable model |
| `unknown kokoro voice 'X' (known: ...)` | bad kokoro voice name |
| `voice 'X' resolves to sid N, but the model has only M speaker(s)` | voice exists, sid out of range |
| `this piper model has N speakers; use voice "sid:0".."sid:N"` | multi-speaker piper without `sid:` |
| `network plugin call failed: ...` | `network` not registered / IPC error |
| `network plugin error: ...` | `network` returned an action error (SSRF block, timeout, DNS) |
| `provider returned HTTP 4xx/5xx: <body>` | the cloud provider rejected the request |
| `malformed base64 response body: ...` | provider returned broken audio encoding |
| `tts_speak is sherpa-only for now, got: X` | `tts_speak` with a cloud provider |
| `missing required field: target` / `target must not be empty` | `tts_speak` without a stream destination |
| `unsupported opus sample rate N (use 8000/12000/16000/24000/48000)` | `tts_speak` with a non-Opus rate |
| `opus encoder init failed: ...` / `opus encode failed: ...` | Opus library error (rare) |
| `failed to stream audio chunk to 'X': ...` | `target` unreachable / IPC error mid-stream |

## Common patterns

- **Read `tts_voices` first**, cache the list, then synthesize — avoids a
  per-call round trip and surfaces misconfiguration early.
- **Local = private.** `sherpa` never touches the network; the audio never
  leaves the machine. Use it for anything sensitive or high-volume.
- **Cloud = convenience.** `openai`/`elevenlabs` for voices the local
  model can't do. Both normalize to the same response shape, so callers
  can switch providers with a one-field change.
- **WAV for analysis, MP3 for storage.** `wav`/`pcm` responses carry real
  `sample_rate_hz`/`num_channels`/`duration_seconds`; MP3 bodies don't.
- **Synthesize is sequential.** The plugin handles one action at a time
  (same as `network`/`ai`); long local texts block briefly. Keep `text`
  short per call and fan out from your side if you need parallelism.
- **Stream to devices with `tts_speak`.** Use it instead of
  `tts_synthesize` when the destination is a remote speaker: Opus over
  `AudioStreamChunk` keeps the wire compact (32 kbps vs ~384 kbps raw
  PCM at 24 kHz). The receiving peer must accept `AudioStreamChunk`
  envelopes and decode Opus — e.g. the Android device agent's speaker
  capability (D-14).
- **`tts_speak` is fire-and-forget.** No ack per chunk; treat a missing
  terminal `end_of_stream` chunk as a failed delivery.
