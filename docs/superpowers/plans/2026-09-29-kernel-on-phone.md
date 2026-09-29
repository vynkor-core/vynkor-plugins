# Vynkor kernel on the Mi 6 — runbook (as executed 2026-09-29)

Sub-project 2 of the `phone` work (see `../specs/2026-09-29-phone-plugin-design.md`).
Goal: a full `vyn` kernel running **on the phone**, so the phone can become the
primary host. This is the record of what was actually done and verified, plus the
known limits. Nothing here touches the phone's read-only rootfs: everything is
user-local under `/home/phablet`.

## Facts that shaped it

| Fact (verified on the device) | Consequence |
|---|---|
| Kernel `4.4.153-Halium+`, aarch64, glibc 2.39, 5.8 GB RAM, 12 GB free on `/home` | Fits easily (kernel RSS ≈ 5 MB, `phone` plugin ≈ 1 MB) |
| No Landlock (`syscall 444` → `ENOSYS`), seccomp-filter + user/PID namespaces present | Plugins need `sandbox: false` (the kernel's `fsaccess.rs` fails closed without Landlock) — user decision |
| No `gcc` on the phone, no `sudo` password needed for any step below | Use the official **static musl** release, not a phone-side build |
| `vynkor` v0.1.3 ships `vyn-aarch64-unknown-linux-musl.tar.gz` (+ `vynm`), `install.sh` supports aarch64 | Installed with the project's own installer (SHA256-verified) |
| Kernel supervisor gives every plugin `RLIMIT_AS = max_vmem_mb` and children inherit it | `phone` needs `max_vmem_mb: 2048` (helper segfaults at 512 MB, fine at ≥ 1024 MB) |

## Steps executed

1. **Back up the old test config** (`port: 8888`, weak secret, `allow_no_auth`):
   `mv ~/.config/vyn/config.yaml ~/.config/vyn/config.yaml.pre-install-2026-09-29`.
2. **Install kernel + manager** with the release installer (checked identical to the repo's
   `install.sh`), user-local: `vyn`, `vyn-pair`, `vynm` in `~/.local/bin`, a fresh
   `~/.config/vyn/config.yaml` with a random `jwt_secret`, port 8000.
3. **Cross-build the `phone` plugin** on the laptop (static, 3.2 MB): `aarch64-unknown-linux-musl`,
   C parts (`zstd-sys`) compiled with `zig cc` from a pip-installed `ziglang` in a venv, linked with
   `rust-lld`:
   ```bash
   export CC_aarch64_unknown_linux_musl=<wrapper: zig cc -target aarch64-linux-musl, drops --target=…>
   export AR_aarch64_unknown_linux_musl=<wrapper: zig ar>
   export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
   RUSTFLAGS="-C linker-flavor=ld.lld" cargo build --release --target aarch64-unknown-linux-musl \
     --manifest-path plugins/phone/Cargo.toml
   ```
   (`zig cc` as the *linker* fails on `--fix-cortex-a53-843419`; `rust-lld` links fine.)
4. **Deploy the plugin**: `~/.local/lib/vyn/plugins/phone/{phone,plugin.json}`.
5. **Drop-in** `~/.config/vyn/plugins.d/phone.yaml` (mode 0600 — it holds a token):
   `sandbox: false`, `max_vmem_mb: 2048`, `PHONE_PLUGIN_TRANSPORT=local`,
   `PHONE_PLUGIN_REMOTE_UID=32011`, `PHONE_PLUGIN_DIR=/home/phablet/.local/share/vyn/phone`,
   and `VYN_JWT_TOKEN` minted on the phone:
   `vyn token -c ~/.config/vyn/config.yaml mint --device phone --permissions PERMISSION_NETWORK,PERMISSION_SCREEN --ttl-seconds 31536000`
   (the token's `sub` must equal the plugin id; a supervised plugin gets its `VYN_JWT_SECRET` injected).
   Without the token the plugin restart-loops with `auth error: missing JWT token`.
6. **systemd user unit** `~/.config/systemd/user/vyn.service`
   (`ExecStart=%h/.local/bin/vyn start --foreground --config %h/.config/vyn/config.yaml`,
   `Restart=on-failure`, `KillMode=control-group`), `systemctl --user enable --now vyn.service`.
   Note: `vyn` 0.1.3 does not find `~/.config/vyn/config.yaml` by itself — always pass `--config`.

## Verified through the kernel on the phone

Driven from the laptop with `scripts/vyn-act` over an ssh-forwarded socket
(`ssh -L /run/user/1000/pk.sock:/run/user/32011/vyn.sock mi6`, `VYN_SOCKET_PATH=…`;
short path needed, unix sockets cap at 108 bytes). vyn-act credentials minted on the phone with
`--device vyn-act --permissions PERMISSION_IPC_SEND,PERMISSION_EVENT_PUBLISH,PERMISSION_NETWORK,PERMISSION_SCREEN`
plus `vyn token … plugin-secret --plugin vyn-act` (a caller must hold the permissions of the
actions it calls).

| Call | Result |
|---|---|
| `phone_status` | `transport: local`, helper ok, 11–16 ms |
| `phone_photo` back 1280x720 / front 640x480 | JPEGs, ≈ 2.8 s each |
| `phone_stream_start` (+`record`) → status → stop | 54 frames in 5 s (10.9 fps, dim room), 1.9 MB record, camera released |
| `phone_photo` during the stream | 7 ms, `source: stream` |
| bad params | `ERR_PHONE_BAD_PARAMS` |

## Known limits / follow-ups

- **Reboot not tested** (unit is enabled; the phone was not rebooted). The user manager on Ubuntu
  Touch starts with the phablet session; without a session the kernel will not run. `loginctl
  enable-linger phablet` needs root and was not done.
- **Exposure**: the kernel listens on `0.0.0.0:8000` (HTTPS, self-signed, JWT required) — reachable
  over wifi at `192.168.31.120`. Add `bind: 127.0.0.1` to `config.yaml` to keep it local; leave it
  open for `vyn device connect` pairing.
- **Only the `phone` plugin is deployed.** Other plugins (`mic`, `sound`, `system`, …) have not been
  built for aarch64-musl or tried on the phone; `mic` already lists `parec` in its recorder chain and
  Pulse is present, so it is the first candidate. `vynm install <slug>` can fetch signed archives —
  whether registry plugins ship aarch64 builds was not checked.
- `sandbox: false` on every plugin on this phone until the kernel grows a Landlock-optional mode.
- Kernel logs `no writable cgroup v2 subtree with the pids controller — falling back to RLIMIT_NPROC`
  (harmless). On shutdown the supervisor restarts plugins a few times before exiting (kernel quirk).
- The old test config is kept as `~/.config/vyn/config.yaml.pre-install-2026-09-29`.
