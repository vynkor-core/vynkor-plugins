#!/usr/bin/env python3
"""Copy plugin state from this machine's vyn install to the phone's kernel.

    scripts/phone-migrate-data.py [--host mi6] [--src ~/.local/share/vyn] [--dry-run]

What moves (consistent SQLite snapshots via the backup API, so WAL content is included):
  plugin-data/database/{agent,automations,calendar,contacts,notes,scheduler,tasks,uptime}.db
  plugin-data/vector-db/agent.db          (agent memory vectors)
  plugin-data/sync/sync.db
  plugin-data/secrets/*.vault             (only readable on the phone with the SAME
                                           SECRETS_PLUGIN_MASTER_KEY: deploy secrets with --keep-master-key)
  telegram/<account>.session              (accounts named in --tg-accounts)

What does NOT move: metrics.db (per-host history), test artefacts (smoke-mem, tester-*, chk-*),
events.db and devices.json (kernel state tied to this machine's jwt_secret), data/plugins/ai/ai.db.

The phone kernel must be STOPPED first (the plugins hold the files open), and the old phone data is
moved aside to <dir>.pre-migrate-<unix time>. Values are never printed.
"""
import argparse
import os
import shlex
import shutil
import sqlite3
import subprocess
import tempfile
import time

PHONE_HOME = "/home/phablet"
DATABASE = ["agent", "automations", "calendar", "contacts", "notes", "scheduler", "tasks", "uptime"]


def ssh(host, cmd, stdin=None):
    r = subprocess.run(["ssh", "-o", "BatchMode=yes", host, cmd], input=stdin, capture_output=True)
    if r.returncode != 0:
        raise SystemExit(f"ssh failed ({r.returncode}): {r.stderr.decode(errors='replace')[:300]}")
    return r.stdout


def snapshot(src_path, dst_path):
    src = sqlite3.connect(f"file:{src_path}?mode=ro", uri=True)
    dst = sqlite3.connect(dst_path)
    src.backup(dst)
    dst.execute("pragma journal_mode=delete")
    dst.close()
    src.close()


def push(host, local, remote, mode):
    d = os.path.dirname(remote)
    with open(local, "rb") as f:
        data = f.read()
    ssh(host, f"mkdir -p {shlex.quote(d)} && cat > {shlex.quote(remote)}.new && chmod {mode} "
              f"{shlex.quote(remote)}.new && mv {shlex.quote(remote)}.new {shlex.quote(remote)}", stdin=data)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="mi6")
    ap.add_argument("--src", default=os.path.expanduser("~/.local/share/vyn"))
    ap.add_argument("--tg-accounts", default="loner42")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()

    plan = []  # (local snapshot/file, phone path relative to ~/.local/share/vyn, mode)
    tmp = tempfile.mkdtemp(prefix="phone-migrate-")
    try:
        for name in DATABASE:
            out = os.path.join(tmp, f"database-{name}.db")
            snapshot(f"{a.src}/plugin-data/database/{name}.db", out)
            plan.append((out, f"plugin-data/database/{name}.db", 600))
        out = os.path.join(tmp, "vector-agent.db")
        snapshot(f"{a.src}/plugin-data/vector-db/agent.db", out)
        plan.append((out, "plugin-data/vector-db/agent.db", 600))
        out = os.path.join(tmp, "sync.db")
        snapshot(f"{a.src}/plugin-data/sync/sync.db", out)
        plan.append((out, "plugin-data/sync/sync.db", 600))
        vdir = f"{a.src}/plugin-data/secrets"
        for v in sorted(os.listdir(vdir)):
            if v.endswith(".vault"):
                plan.append((os.path.join(vdir, v), f"plugin-data/secrets/{v}", 600))
        for acc in filter(None, a.tg_accounts.split(",")):
            out = os.path.join(tmp, f"tg-{acc}.session")
            snapshot(f"{a.src}/telegram/{acc}.session", out)
            plan.append((out, f"telegram/{acc}.session", 600))

        for local, rel, _ in plan:
            print(f"{os.path.getsize(local):>9}  {rel}")
        if a.dry_run:
            return

        active = ssh(a.host, "systemctl --user is-active vyn.service || true").decode().strip()
        if active == "active":
            raise SystemExit("phone vyn.service is running: stop it first (systemctl --user stop vyn.service)")

        stamp = int(time.time())
        base = f"{PHONE_HOME}/.local/share/vyn"
        for _, rel, _ in plan:
            dst = shlex.quote(f"{base}/{rel}")
            # keep whatever the phone had under <file>.pre-migrate-<t>; stale WAL/SHM would be
            # replayed onto the fresh snapshot, so they go too
            ssh(a.host, f"[ -e {dst} ] && mv {dst} {dst}.pre-migrate-{stamp}; rm -f {dst}-wal {dst}-shm; true")
        for local, rel, mode in plan:
            push(a.host, local, f"{base}/{rel}", mode)
        print("pushed", len(plan), "files; replaced phone files kept as *.pre-migrate-%d" % stamp)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
