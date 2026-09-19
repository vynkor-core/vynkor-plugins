# speech ROADMAP

## Deployment status (2026-09-19)

Installed and live, replacing the standalone `stt` and `tts` plugins on
this machine — those two declared the same action names (`stt_transcribe`,
`tts_speak`, etc.), so keeping all three registered at once would have hit
a duplicate-action-name situation the kernel's actual behavior for is
unverified (see `plugins/agent/ROADMAP.md` "Known issues"). `stt`/`tts`
were moved to `~/.config/vyn/plugins.d/{stt,tts}.yaml.disabled` and their
binaries backed up under `~/.local/lib/vyn/plugins.bak-<ts>/` rather than
deleted, in case `speech` needs to be rolled back. This does not touch the
sherpa hang below — `speech` reuses the same engines verbatim, so it has
the identical failure mode under the supervisor.

## Known issue: sherpa inference never completes under the kernel supervisor

**Status:** open, environment-level (not specific to the `speech` merge —
standalone `tts` reproduces identically). Needs a debugging session with
`strace`/`gdb` attached to the `__shim` → plugin pair.

**Symptom.** Under the supervisor, the first local-sherpa action loads the
model (RSS grows to ~160 MB for piper medium, CPU burns during load) and
then never returns: synthesis never completes, all threads sit in
`futex_do_wait`, 0% CPU afterwards. The caller times out. Repeats for
`tts_speak`, `tts_speak_stream`, `tts_synthesize`, and the stt actions once
they need the engine.

**Ruled out** (repro matrix, 2026-08-26):

| Variant | Result |
|---|---|
| Same binary+env+model outside the kernel (`cargo run`) | ✅ 80–100 ms TTFA, full paragraph 2.3–3.8 s |
| …with `ulimit -v 4096 MB` | ✅ identical — RLIMIT_AS size is not the trigger |
| Supervised, sandbox true/false | ❌ hangs |
| Supervised, `RLIMIT_AS` 512M / 1536M / 4096M | ❌ 512M = hang at model alloc; ≥1536M = model loads, then livelock |
| Supervised, `TTS_PLUGIN_LOCAL_NUM_THREADS=1` | ❌ hangs |
| Kernel cwd changed | ❌ hangs |
| stdout/stderr pipe draining | ✅ verified continuous (`drain_to_log`), not backpressure |
| seccomp filter on child | none (`Seccomp: 0`) |
| Merge-specific code | ❌ standalone `tts` reproduces byte-for-byte behaviour |

**Remaining suspects** (in order): something in the supervisor/shim spawn
path that the manual runs lack — e.g. fd/pid namespace details of `__shim`,
an inherited signal disposition, or an ONNX-runtime interaction with the
cgroup-v2 membership done in `pre_exec`. Next steps: attach `strace -f` to
the shimmed plugin during a call (compare syscall streams vs standalone);
diff `/proc/<pid>/status` and `/proc/<pid>/stat` fully between manual and
shimmed runs before the first synthesis; try ONNX verbosity
(`ORT_LOG_LEVEL=VERBOSE` style env) inside the shimmed run.

**Workaround until fixed:** voice plugins that need the local engine are
best run against a kernel built without the resource-capping pre_exec
(dev kernels), or use cloud providers (`openai`/`elevenlabs`), which never
touch sherpa. The merge itself is sound: engines are verbatim copies, and
synthesis outside the kernel is fast and correct.
