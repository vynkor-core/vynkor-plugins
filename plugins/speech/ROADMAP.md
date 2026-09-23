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

**Status: no longer reproduces (verified live, 2026-09-23).** Called the
deployed `speech` plugin's `tts_synthesize` (`provider: sherpa`, piper model)
three times via `vyn-act` against the running kernel: cold call 2.26s
(RSS 4.9MB → 136MB, matching the ~160MB piper-medium footprint this doc
recorded during the original repro), two warm calls 295ms and 109ms
(wav and mp3), all returned valid audio, process idle at 0% CPU afterward —
not livelocked. `ps` also shows `speech` spawned as a **direct child of the
kernel process, no `__shim` wrapper in between** (unlike every sandboxed
plugin, which shows `vyn __shim <path>` as the parent) — this machine's
`speech.yaml` has `sandbox: false`, and the original repro matrix's "sandbox
true/false, both hang" row must predate whatever kernel change stopped
routing `sandbox: false` plugins through `__shim` at all. Not re-tested with
`sandbox: true` — if a future edit sets it, re-verify before assuming the
fix generalizes. Kept below for history; reopen with a fresh repro if it
resurfaces.

**Status (historical, until 2026-09-23):** open, environment-level (not
specific to the `speech` merge — standalone `tts` reproduces identically).
Needed a debugging session with `strace`/`gdb` attached to the `__shim` →
plugin pair.

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

**Workaround (historical, may be unnecessary now — see Status above):** voice
plugins that need the local engine are best run against a kernel built
without the resource-capping pre_exec (dev kernels), or use cloud providers
(`openai`/`elevenlabs`), which never touch sherpa. The merge itself is
sound: engines are verbatim copies, and synthesis outside the kernel is
fast and correct.
