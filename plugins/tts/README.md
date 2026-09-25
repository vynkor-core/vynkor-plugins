# tts plugin

Text-to-speech for Vynkor plugins. Exposes three actions: `tts_synthesize`
(turn text into audio), `tts_voices` (list selectable voices), and
`tts_speak` (stream Opus audio to a peer plugin — the D-12 voice
pipeline's host-TTS → client-speaker leg).

Three providers behind one normalized interface:

| Provider | Where it runs | What it is |
|---|---|---|
| `sherpa` | **in-process** (local) | sherpa-onnx ONNX inference — Kokoro-82M and Piper voices, fully offline |
| `openai` | cloud, via `network` | OpenAI TTS (`tts-1` / `tts-1-hd` / `gpt-4o-mini-tts`) |
| `elevenlabs` | cloud, via `network` | ElevenLabs TTS (multilingual v2) |

Cloud providers route every request through the `network` plugin's
`http_request` action, so `network` must also be registered and running
for them (same model as `ai`). `sherpa` opens no sockets — it loads an
ONNX model from disk and synthesizes in-process, so it works with nothing
but the kernel and the model files.

**See [`USAGE.md`](./USAGE.md)** for the caller-facing guide: full
`tts_synthesize` / `tts_voices` request/response reference, per-provider
examples, every error message a caller can hit, and common patterns.

## Operator note

`tts` declares three kernel permissions — `network`, `audio_stream`, and
`secrets` (`plugin.json`: `"permissions": ["network",
"PERMISSION_AUDIO_STREAM", "PERMISSION_IPC_SEND", "secrets"]`).
`network` because its cloud providers invoke the `network` plugin's gated
`http_request` action, and the kernel's anti-laundering check (T-19)
requires callers of a gated action to hold its permission too (Manifest
v2). `audio_stream` because `tts_speak` streams `AudioStreamChunk`s to a
peer (proto v1.6 `PERMISSION_AUDIO_STREAM`). `secrets` because cloud
providers resolve their API keys from the `secrets` plugin's vault first
(gated `secret_get`; T-19 again). It opens no sockets itself,
so it's safe to run with `sandbox: true`. `network` still needs
`sandbox: false` (real egress) for the cloud providers — see
`plugins/network/README.md`.

The local provider loads a model into RAM at first use; size `max_vmem_mb`
above the model size (Kokoro f32 ≈ 310 MB, int8 ≈ 88 MB; piper medium ≈
100 MB). The kernel default is 2048 MiB (raised from 512 in 2026-08-26
because ONNX Runtime reserves ~500 MiB of virtual address space at init);
`0` means unlimited. See `config.example.yaml`.

### Build dependency: libmp3lame

`tts` links the C LAME encoder via the `mp3lame-encoder` crate (pinned
`=0.2.5`, LGPL-3.0) to encode sherpa's PCM into MP3 for `format: "mp3"`.
LAME is a **build-time** C dependency: the `libmp3lame` development
library and headers must be installed to compile (`libmp3lame-dev` on
Debian/Ubuntu, `lame-devel` on Fedora). At runtime the plugin links the
shared `libmp3lame.so` — no separate process or daemon.

## Action: `tts_synthesize`

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

- `provider` — `"sherpa"` | `"openai"` | `"elevenlabs"`. Required.
- `text` — required, 1..=4000 chars.
- `voice` — required. `sherpa`: Kokoro name (`af_heart`, ...) or `sid:N`;
  `openai`: one of the documented voice ids; `elevenlabs`: your account's
  voice id.
- `api_key_env` — required for cloud providers. The env-var-style name is
  a lookup handle, never a literal key: `tts` reads it from the `secrets`
  plugin's vault first (under that exact name), then falls back to the
  process environment. Must be on the operator's
  `TTS_PLUGIN_ALLOWED_KEY_ENVS` allowlist. Ignored for `sherpa`.
- `format` — optional. `sherpa`: `wav` (default) | `pcm` | `mp3`;
  `openai`: `mp3` (default) | `wav` | `pcm` | `opus` | `aac` | `flac`;
  `elevenlabs`: `mp3` (default) | `pcm` | `ulaw`.
- `speed` — optional, `0.25`..=`4.0`, default `1.0`.
- `timeout_ms` — optional, default/cap `60000`. Cloud requests are
  additionally capped at `network`'s own 30 s HTTP limit.
- `base_url`, `model` — optional per-provider overrides (defaults:
  `https://api.openai.com/v1` / `gpt-4o-mini-tts`,
  `https://api.elevenlabs.io` / `eleven_multilingual_v2`).

Response (`ActionResponse.data_json`) on success, normalized across all
providers:

```json
{
  "format": "wav",
  "sample_rate_hz": 24000,
  "num_channels": 1,
  "duration_seconds": 2.4,
  "audio_base64": "UklGR..."
}
```

`sample_rate_hz` / `num_channels` / `duration_seconds` are `0` when the
container can't carry them (mp3/opus/aac/flac from a cloud provider);
`ulaw` reports `sample_rate_hz` 8000 (its fixed rate). WAV, raw-PCM, and
sherpa-local mp3 bodies carry real values (sherpa knows the source rate,
channels, and duration from the PCM it encoded). Decode `audio_base64`
for the bytes.

Errors → `ACTION_ERROR` with a human-readable message: malformed/missing
request fields, unknown voice, model load failure (missing/wrong model
files), un-allowlisted or unset `api_key_env`, non-2xx HTTP status from a
provider, or any error `network`'s `http_request` itself returns.

## Action: `tts_voices`

```json
{ "provider": "sherpa" }
```

Returns the voices the provider exposes:

```json
[
  { "id": "af_heart", "name": "af_heart" },
  { "id": "sid:26", "name": "speaker 26" }
]
```

- `sherpa` — real list from the loaded model (Kokoro names, plus `sid:N`
  for any extra speakers).
- `openai` — the documented voice id list.
- `elevenlabs` — rejected: voices are per-account; list them via the
  ElevenLabs dashboard or `GET /v1/voices`.

## Action: `tts_speak`

Synthesize and **stream** the result as Opus to a peer plugin — the D-12
host-TTS → client-speaker leg. Local (`sherpa`) only: the cloud providers
return opaque containers with no PCM source to encode.

```json
{
  "provider": "sherpa",
  "text": "Hello from the host.",
  "voice": "af_heart",
  "target": "phone-1.speaker"
}
```

See `USAGE.md` for the full field reference. In short: `target` names the
receiving peer, the audio goes out as `AudioStreamChunk` envelopes (codec
`OPUS`, 20 ms frames, `end_of_stream` on the last), and the action
response is a summary (`codec`/`stream_id`/`packets`/`duration_seconds`).
Requires `PERMISSION_AUDIO_STREAM`.

`tts_speak_stream` (EXI-02) takes the same request shape but splits the text
into sentences (RU/EN abbreviation-aware) and streams each as it finishes —
first audio lands after one phrase, not after the whole paragraph; only the
last packet carries `end_of_stream`. See USAGE.md.

## Configuration

`tts` reads no config file itself — settings are environment variables
set in the kernel's `config.yaml`, under this plugin's `env:` list — see
`config.example.yaml` in this directory. Provider API keys may also be
stored in the `secrets` plugin's vault instead (see below).

- `TTS_PLUGIN_ALLOWED_KEY_ENVS` — **required for cloud providers**: a
  comma-separated, exact-match allowlist of every env var name a caller's
  `api_key_env` may reference. Default-deny — without it every cloud
  `tts_synthesize` request is rejected. Same rationale as `ai`'s
  `AI_PLUGIN_ALLOWED_KEY_ENVS`: without the allowlist a caller could name
  *any* env var the `tts` process has and have its value sent straight into
  an outbound request header to a caller-controlled `base_url`.
- **Provider API keys** — resolved secrets-first at call time, never baked
  into the request: `tts` asks the `secrets` plugin's vault
  (`secret_get`) for the key under the `api_key_env` name, then falls
  back to a process env var of that name (see `config.example.yaml`).
  The vault wins when both hold a value; a missing vault is logged and
  the env fallback is used. The `secrets` plugin must be registered for
  the vault hop.
- `TTS_PLUGIN_LOCAL_MODEL_DIR` — **required for `sherpa`**: directory with
  the ONNX model files.
- `TTS_PLUGIN_LOCAL_MODEL_TYPE` — **required for `sherpa`**: `kokoro` or
  `piper`.
- `TTS_PLUGIN_LOCAL_NUM_THREADS` — optional, default `2`.
- `TTS_PLUGIN_KOKORO_LEXICON` — optional comma-separated lexicon list;
  default auto-detects every `lexicon-*.txt` in the model dir.

### Setting up a local model

**Kokoro** (best quality open voices, 26 voices across 9 languages).
Create `TTS_PLUGIN_LOCAL_MODEL_DIR` with:

```
model.onnx          # Kokoro-82M ONNX (f32 ~310 MB, or int8 ~88 MB from taylorchu/kokoro-onnx)
voices.bin          # voice style vectors (voices-v1.0.bin from the same release)
tokens.txt          # tokenizer vocab (from the sherpa-onnx kokoro model pack)
espeak-ng-data/     # espeak-ng phoneme data (from any sherpa-onnx release zip)
```

For non-English text, also drop in `lexicon-*.txt` and `dict/` (the
sherpa-onnx kokoro-multi-lang model pack ships both; they're auto-detected
— an English-only install doesn't need them).

**Piper** (small, fast, many languages). Create the dir with:

```
model.onnx          # e.g. en_US-lessac-medium from rhasspy/piper-voices
tokens.txt          # same voice's tokens.txt
espeak-ng-data/     # phoneme data (extract from any rhasspy/piper release zip)
```

The model is loaded lazily on the first `sherpa` synthesize request and
cached for the process lifetime.

## Testing

`cargo test` — 75 unit tests, no live network and no model files required
(providers are tested against fixture audio/JSON; sherpa config assembly
is tested without loading a real model; the Opus and MP3 encoders are
tested with encode/decode and frame-sync checks). End-to-end behavior was
verified against a real kernel + `network` + `tts` + local Kokoro stack;
there's no automated integration test for that yet.
