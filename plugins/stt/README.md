# stt plugin

> **Legacy.** New installs should use [`speech`](../speech/), which runs
> this engine verbatim in one process alongside `tts`. Both declare the
> same action names, and the kernel refuses to route an action declared by
> two plugins — run `speech` *or* the standalone `stt`+`tts`, never both.

Speech-to-text for Vynkor plugins. Exposes four actions: `stt_transcribe`
(turn audio into text), `stt_models` (list transcribable models), and
`stt_listen_start`/`stt_listen_stop` (stream PCM in, get a transcript out
as an event — the D-12 voice pipeline's client-STT → host-text leg).

Two providers behind one normalized interface:

| Provider | Where it runs | What it is |
|---|---|---|
| `sherpa` | **in-process** (local) | sherpa-onnx ONNX inference — zipformer/whisper offline models, fully offline |
| `openai` | cloud, via `network` | OpenAI Audio API (`whisper-1` / `gpt-4o-transcribe` / `gpt-4o-mini-transcribe`) |

The cloud provider routes every request through the `network` plugin's
`http_request` action, so `network` must also be registered and running
for it (same model as `ai` and `tts`). `sherpa` opens no sockets — it
loads an ONNX model from disk and transcribes in-process, so it works
with nothing but the kernel and the model files. The listen path
(`stt_listen_*`) is local-only by design: audio streamed from a mic peer
never leaves the device; only the transcript is published as an event.

**See [`USAGE.md`](./USAGE.md)** for the caller-facing guide: full
`stt_transcribe` / `stt_models` / `stt_listen_start` / `stt_listen_stop`
request/response reference, per-provider examples, every error message a
caller can hit, and common patterns.

## Operator note

`stt` declares four permissions (`plugin.json`:
`"permissions": ["network", "secrets", "PERMISSION_AUDIO_STREAM", "PERMISSION_EVENT_PUBLISH"]`).
`network` because its cloud provider invokes the `network` plugin's gated
`http_request` action, and the kernel's anti-laundering check (T-19)
requires callers of a gated action to hold its permission too (Manifest
v2). `secrets` because the cloud provider resolves its API key through
the `secrets` plugin's gated `secret_get` action first (the env var is
only the fallback). `audio_stream` because the listen path receives
`AudioStreamChunk` PCM from a mic peer, and `event_publish` because
`stt_listen_stop` publishes the transcript as an `stt_text` event (both
proto v1.6). It opens no sockets itself, so it's safe to run with
`sandbox: true`.
`network` still needs `sandbox: false` (real
egress) for the cloud provider — see `plugins/network/README.md`.

The local provider loads a model into RAM at first use; size `max_vmem_mb`
above the model size (zipformer int8 ≈ 50-100 MB; whisper tiny/base a few
hundred MB). See `config.example.yaml`.

## Action: `stt_transcribe`

Request (`ActionRequest.params_json`):

```json
{
  "provider": "sherpa",
  "audio_base64": "UklGRgAAAABXQVZF..."
}
```

- `provider` — `"sherpa"` | `"openai"`. Required.
- `audio_base64` — required, base64 of the audio bytes, ≤ 25 MiB after
  decoding.
- `format` — optional. `sherpa`: `wav` (default) | `pcm` (raw 16-bit,
  requires `sample_rate_hz` + `num_channels`); `openai`: `wav` (default) |
  `mp3` | `ogg`.
- `sample_rate_hz`, `num_channels` — required for `sherpa` with `pcm`
  format; ignored otherwise.
- `language` — optional ISO-639-1 hint (e.g. `"de"`). Caller-declared;
  echoed back in the response, and sent to the provider for `openai` /
  applied per-request for `sherpa` whisper models.
- `prompt` — optional Whisper-style context hint (`openai` only), ≤ 1000
  chars.
- `temperature` — optional, `0.0`..=`1.0` (`openai` only).
- `api_key_env` — required for `openai` (never a literal key; must be on
  the operator's `STT_PLUGIN_ALLOWED_KEY_ENVS` allowlist). Resolved
  secrets-first: the key is read from `stt`'s own `secrets` vault under
  this exact name, with the same-named env var of the `stt` process as
  fallback — the vault wins when both exist. Ignored for `sherpa`.
- `timeout_ms` — optional, default/cap `60000`. Cloud requests are
  additionally capped at `network`'s own 30 s HTTP limit.
- `base_url`, `model` — optional per-provider overrides (defaults:
  `https://api.openai.com/v1` / `whisper-1`; for `openai`, `model` must be
  one of the ids `stt_models` lists).

Response (`ActionResponse.data_json`) on success, normalized across both
providers:

```json
{
  "text": "Hello from Vynkor.",
  "language": "en",
  "duration_seconds": 2.4,
  "model": "sherpa:transducer"
}
```

`language` is `""` when unknown; `duration_seconds` is `0` when it can't
be derived from the container format (e.g. an mp3/ogg upload). `model` is
the provider's model id (for `openai`, the resolved model; for `sherpa`,
`sherpa:<family>`).

Errors → `ACTION_ERROR` with a human-readable message: malformed/missing
request fields, unknown model, model load failure (missing/wrong model
files), un-allowlisted or unset `api_key_env`, non-2xx HTTP status from a
provider, or any error `network`'s `http_request` itself returns.

## Action: `stt_models`

```json
{ "provider": "sherpa" }
```

Returns the models the provider exposes:

```json
[
  { "id": "sherpa:transducer", "name": "local sherpa-onnx model (transducer)" }
]
```

- `sherpa` — the single operator-configured model.
- `openai` — the known model id list (`whisper-1`, `gpt-4o-transcribe`,
  `gpt-4o-mini-transcribe`).

## Actions: `stt_listen_start` / `stt_listen_stop`

Stream-based transcription for the D-12 voice pipeline: a mic peer sends
`AudioStreamChunk` envelopes (codec `PCM_S16LE`), and `stt` accumulates,
then transcribes locally and publishes the text as an `stt_text` event
(namespaced `plugin.stt.stt_text`). The audio never leaves the device —
only the transcript does.

```json
{ "stream_id": 1, "sample_rate_hz": 16000 }
```

See `USAGE.md` for the full reference. In short: `stt_listen_start` opens
a per-`stream_id` buffer (rate locked, channels downmixed to mono),
inbound PCM chunks fill it, and `stt_listen_stop` transcribes with the
local sherpa model, publishes the transcript event, and returns the text.
Requires `PERMISSION_AUDIO_STREAM` and `PERMISSION_EVENT_PUBLISH`.

### Voice-activity events (opt-in)

With `STT_PLUGIN_VAD=on`, every chunk also advances an energy VAD
(per-chunk RMS with hysteresis) and the listen path publishes speech
boundaries:

| Namespaced event | Payload | Meaning |
|---|---|---|
| `plugin.stt.stt_speech_started` | `{"stream_id": N}` | two consecutive loud chunks opened an utterance |
| `plugin.stt.stt_speech_ended` | `{"stream_id": N, "speech_ms": M}` | `SILENCE_MS` of quiet closed it after ≥ `MIN_SPEECH_MS` of speech |

This is the endpoint orchestrators key on — the daemon's
`DAEMON_PLUGIN_MODE=vad` ends a voice turn on `stt_speech_ended` instead
of a fixed capture window. The VAD is deliberately primitive (it solves
"when did the user stop talking", not speaker diaristics); all thresholds
are env-tunable and it is off by default, so streams behave exactly as
before unless enabled. Too-short blips reset quietly without an ending
event.

## Configuration

`stt` reads no config file itself — environment variables set in the
kernel's `config.yaml`, under this plugin's `env:` list — see
`config.example.yaml` in this directory.

- `STT_PLUGIN_ALLOWED_KEY_ENVS` — **required for the cloud provider**: a
  comma-separated, exact-match allowlist of every env var name a caller's
  `api_key_env` may reference. Default-deny — without it every cloud
  `stt_transcribe` request is rejected. Same rationale as `ai`'s
  `AI_PLUGIN_ALLOWED_KEY_ENVS` and `tts`'s `TTS_PLUGIN_ALLOWED_KEY_ENVS`.
- The provider key itself is resolved secrets-first: `stt` looks it up in
  its own `secrets` vault under the env-var name (`secret_set {"name":
  "OPENAI_API_KEY", "value": "sk-..."}`), and falls back to the env var
  of the same name. The vault wins when both exist. `secrets` must be
  registered and running for the vault hop; without it the env var is
  used.
- `STT_PLUGIN_LOCAL_MODEL_DIR` — **required for `sherpa`**: directory with
  the ONNX model files.
- `STT_PLUGIN_LOCAL_MODEL_TYPE` — **required for `sherpa`**: `transducer`
  or `whisper`.
- `STT_PLUGIN_LOCAL_NUM_THREADS` — optional, default `2`.
- `STT_PLUGIN_LOCAL_LANGUAGE` — optional (whisper family only), default
  `"en"`.

### Setting up a local model

**Transducer (zipformer)** — solid accuracy for English and several other
languages; medium-sized. Create `STT_PLUGIN_LOCAL_MODEL_DIR` with:

```
encoder.onnx         # from a sherpa-onnx-zipformer-* model pack
decoder.onnx
joiner.onnx
tokens.txt
```

**Whisper** — classic Whisper accuracy, converted to ONNX. Create the dir
with:

```
encoder.onnx         # from a sherpa-onnx-whisper-* model pack
decoder.onnx
tokens.txt
```

Model packs download from the k2-fsa/sherpa-onnx releases
(`sherpa-onnx-zipformer-en-2023-06-26`, `sherpa-onnx-whisper-tiny.en`,
etc.). The model is loaded lazily on the first `sherpa` transcribe request
and cached for the process lifetime.

## Testing

`cargo test` — 69 unit tests, no live network and no model files required
(providers are tested against fixture audio/JSON; sherpa config assembly
is tested without loading a real model; the listen accumulator is tested
with synthetic PCM chunks). There's no automated
kernel + `network` + model integration test yet.
