#!/usr/bin/env python3
"""Deploy staged plugins (scripts/phone-build.sh output) to the phone's vyn kernel.

    scripts/phone-deploy.py [--host mi6] [--dist DIR] [--src ~/.config/vyn/plugins.d] PLUGIN...

For every plugin it (1) copies the binary + plugin.json to ~/.local/lib/vyn/plugins/<id>/ on the
phone, (2) writes ~/.config/vyn/plugins.d/<id>.yaml (mode 0600) derived from the same plugin's
drop-in on THIS machine, and (3) mints the plugin's JWT ON THE PHONE with the same permissions.

Derivation rules (never prints a secret value):
  * sandbox -> false            (phone kernel 4.4 has no Landlock)
  * /home/<local user>/ paths   -> /home/phablet/
  * VYN_JWT_TOKEN               -> re-minted on the phone with the local token's permissions
                                   (ipc_targets naming stale dev-test-* devices are dropped)
  * SECRETS_PLUGIN_MASTER_KEY   -> a NEW random key generated on the phone (copied only with
                                   --keep-master-key, needed to open a migrated vault)
  * env names ending in _API_KEY / _TOKEN / _SECRET / _PASS / _PASSWORD / _HASH -> NOT copied; listed
                                   as skipped so the operator sets them on the phone deliberately
                                   (copied verbatim with --with-secrets)
  * every other env entry       -> copied verbatim (after the path rewrite)
The kernel reads plugins.d only at start: restart vyn.service afterwards.
"""
import argparse
import base64
import getpass
import json
import os
import re
import shlex
import subprocess
import sys

import yaml

SECRETISH = re.compile(r"(_API_KEY|_TOKEN|_SECRET|_PASS|_PASSWORD|_HASH|API_HASH)($|_)", re.I)
PHONE_HOME = "/home/phablet"
# Env the derivation rules would drop (secret-named) but the phone needs as a PLACEHOLDER: the
# ai/vector-db plugins insist on an api_key_env even for a local Ollama, which ignores the value
# (cloud-model auth lives in Ollama itself). Never put a real key here.
PLACEHOLDER_ENV = {
    "ai": ["OLLAMA_API_KEY=ollama"],
    "vector-db": ["OLLAMA_API_KEY=ollama", "VECTOR_DB_EMBED_API_KEY_ENV=OLLAMA_API_KEY"],
}
VYN = "PATH=$HOME/.local/bin:$PATH vyn"
CFG = "~/.config/vyn/config.yaml"


def ssh(host, cmd, stdin=None, check=True):
    r = subprocess.run(["ssh", "-o", "BatchMode=yes", host, cmd], input=stdin, capture_output=True)
    if check and r.returncode != 0:
        raise SystemExit(f"ssh failed ({r.returncode}): {r.stderr.decode(errors='replace')[:300]}")
    return r.stdout


def claims_of(token):
    payload = token.split(".")[1]
    payload += "=" * (-len(payload) % 4)
    return json.loads(base64.urlsafe_b64decode(payload))


def mint(host, plugin_id, perms, ipc_targets, ttl):
    cmd = f"{VYN} token -c {CFG} mint --device {shlex.quote(plugin_id)} --ttl-seconds {ttl}"
    if perms:
        cmd += " --permissions " + shlex.quote(",".join(perms))
    if ipc_targets:
        cmd += " --ipc-targets " + shlex.quote(",".join(ipc_targets))
    tok = ssh(host, cmd + " 2>/dev/null").decode().strip().splitlines()[-1].strip()
    if tok.count(".") != 2:
        raise SystemExit(f"{plugin_id}: token mint returned garbage")
    return tok


def push(host, path, data, mode):
    d = os.path.dirname(path)
    ssh(host, f"mkdir -p {shlex.quote(d)} && cat > {shlex.quote(path)}.new && chmod {mode} {shlex.quote(path)}.new "
              f"&& mv {shlex.quote(path)}.new {shlex.quote(path)}", stdin=data)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("plugins", nargs="+")
    ap.add_argument("--host", default="mi6")
    ap.add_argument("--dist", default=os.path.expanduser("~/.cache/vyn-phone-build/dist"))
    ap.add_argument("--src", default=os.path.expanduser("~/.config/vyn/plugins.d"))
    ap.add_argument("--ttl", type=int, default=31536000)
    ap.add_argument("--with-secrets", action="store_true", help="copy secret-named env verbatim")
    ap.add_argument("--keep-master-key", action="store_true", help="copy SECRETS_PLUGIN_MASTER_KEY")
    a = ap.parse_args()
    local_home = os.path.expanduser("~")

    for pid in a.plugins:
        path = os.path.join(a.src, f"{pid}.yaml")
        if not os.path.exists(path):  # plugin disabled on this machine after being moved to the phone
            path += ".disabled"
        src = yaml.safe_load(open(path))
        stage = os.path.join(a.dist, pid)
        binary = src["binary"].rsplit("/", 1)[-1]
        libdir = f"{PHONE_HOME}/.local/lib/vyn/plugins/{pid}"

        push(a.host, f"{libdir}/{binary}", open(os.path.join(stage, binary), "rb").read(), 755)
        push(a.host, f"{libdir}/plugin.json", open(os.path.join(stage, "plugin.json"), "rb").read(), 644)

        env, skipped, token_claims = [], [], None
        for entry in src.get("env") or []:
            k, _, v = str(entry).partition("=")
            if k == "VYN_JWT_TOKEN":
                token_claims = claims_of(v)
            elif k == "VYN_JWT_SECRET":
                continue  # injected per plugin by the supervisor
            elif k == "SECRETS_PLUGIN_MASTER_KEY" and a.keep_master_key:
                env.append(f"{k}={v}")
            elif k == "SECRETS_PLUGIN_MASTER_KEY":
                key = ssh(a.host, "head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \\n'").decode().strip()
                env.append(f"{k}={key}")
            elif SECRETISH.search(k) and not a.with_secrets:
                skipped.append(k)
            else:
                env.append(f"{k}={v.replace(local_home + '/', PHONE_HOME + '/')}")

        for extra in PLACEHOLDER_ENV.get(pid, []):
            name = extra.split("=", 1)[0]
            env = [e for e in env if not e.startswith(name + "=")] + [extra]
            skipped = [k for k in skipped if k != name]

        perms = (token_claims or {}).get("permissions", [])
        ipc = [t for t in (token_claims or {}).get("ipc_targets", []) if not t.startswith("dev-")]
        token = mint(a.host, pid, perms, ipc, a.ttl)
        env.insert(0, f"VYN_JWT_TOKEN={token}")

        out = {
            "id": pid,
            "binary": f"{libdir}/{binary}",
            "restart": src.get("restart", "on-failure"),
            "max_restarts": src.get("max_restarts", 5),
            "sandbox": False,
            "env": env,
        }
        for k in ("max_procs", "max_vmem_mb", "grace_seconds"):
            if src.get(k) is not None:
                out[k] = src[k]
        push(a.host, f"{PHONE_HOME}/.config/vyn/plugins.d/{pid}.yaml", yaml.safe_dump(out, sort_keys=False).encode(), 600)

        # make sure data dirs named in env exist
        dirs = {v.split("=", 1)[1] for v in env if re.search(r"(DATA_DIR|SESSION_DIR)=", v)}
        for d in sorted(dirs):
            ssh(a.host, f"mkdir -p {shlex.quote(d)}")
        note = f" (skipped secret env: {', '.join(skipped)})" if skipped else ""
        print(f"deployed {pid}: perms={len(perms)} ipc_targets={len(ipc)} env={len(env) - 1}{note}")


if __name__ == "__main__":
    main()
