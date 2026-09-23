# speech plugin

`tts` + `stt` merged into one process (PLANS.md MRG-01): one registration,
one binary, one drop-in. The two engines are path-dep sub-crates copied
verbatim from the standalone plugins — every action name (`tts_*`,
`stt_*`, including `tts_speak_stream`), event type (`stt_text`,
speech boundaries), and env knob (`TTS_PLUGIN_*`, `STT_PLUGIN_*`) is
byte-identical, so `daemon`/webclient callers and operator configs need
zero changes beyond the plugin id — but the plugin id *does* matter for
audio routing: set `DAEMON_PLUGIN_STT_TARGET=speech` on `daemon` and
`MIC_PLUGIN_IPC_TARGETS=speech` on `mic`. With the old `stt` values the
kernel drops every mic chunk (no plugin `stt` is registered) and each voice
turn fails with "listen stream N has no audio buffered".

**The standalone `tts` and `stt` plugins remain shipped.** Run either
`speech` alone or the two singles — running all three on one machine
means two owners of the mic/model memory; pick one layout per host.

## Actions

Union of both singles (see their READMEs for full references):
`tts_synthesize`, `tts_voices`, `tts_speak`, `tts_speak_stream`,
`stt_transcribe`, `stt_models`, `stt_listen_start`, `stt_listen_stop`.

## Permissions

`PERMISSION_NETWORK`, `PERMISSION_SECRETS` (cloud providers + vault-first
keys, T-19), `PERMISSION_AUDIO_STREAM` + `PERMISSION_IPC_SEND`
(`tts_speak*` outbound / listen inbound D-12), `PERMISSION_EVENT_PUBLISH`
(`stt_text`, VAD events).

## Configuration

Everything comes from the legacy env vars verbatim — see
`plugins/tts/config.example.yaml` + `plugins/stt/config.example.yaml`
(model dirs/types, voices, key allowlists, `TTS_PLUGIN_IPC_TARGETS`,
`STT_PLUGIN_VAD`). `config.example.yaml` here shows the skeleton. The
kernel's `max_vmem_mb` default is 2048 MiB (0 = unlimited); both sherpa
models may be resident, so keep it ≥ model size + workspace.
