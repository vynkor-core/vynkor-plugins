#!/usr/bin/env python3
"""Auto-approve `needs_confirmation` Telegram sweep goals on THIS kernel (runs on the phone).

Port of the laptop's ~/.local/bin/vyn-auto-approver.py: same title filter, but it mints its own
short-lived credentials from the local kernel config instead of reading an admin_token file, and
finds the socket under $XDG_RUNTIME_DIR. Needs the vynkor python SDK on sys.path (~/pylibs).
"""
import asyncio
import json
import os
import pathlib
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path.home() / "pylibs"))
from vynkor import VynkorClient  # noqa: E402
from vynkor.vynkor_protocol_pb2 import PluginManifest  # noqa: E402

HOME = pathlib.Path.home()
VYN = str(HOME / ".local/bin/vyn")
CFG = str(HOME / ".config/vyn/config.yaml")
SOCK = os.environ.get("VYN_SOCKET_PATH") or f"{os.environ.get('XDG_RUNTIME_DIR', '/run/user/%d' % os.getuid())}/vyn.sock"
PLUGIN = "ops-cli"
PERMS = "PERMISSION_IPC_SEND,PERMISSION_EVENT_PUBLISH,PERMISSION_NETWORK,PERMISSION_SCREEN"
MARKERS = ("tg-", "sweep", "mixed")


def vyn(*args):
    return subprocess.check_output([VYN, "token", "-c", CFG, *args], text=True).strip().splitlines()[-1].strip()


async def main():
    while True:
        try:
            token = vyn("mint", "--device", PLUGIN, "--permissions", PERMS, "--ttl-seconds", "3600")
            secret = vyn("plugin-secret", "--plugin", PLUGIN)  # per-plugin frame-MAC key
            client = await VynkorClient.connect_with_secret(SOCK, secret.encode())
            ack = await client.register_with_token(PLUGIN, PluginManifest(actions=[], action_specs=[]), token)
            if not ack.accepted:
                raise RuntimeError(f"register rejected: {ack}")
            for _ in range(150):  # ~50 min, then re-mint the 1h token
                resp = await client.send_action("goal_list", json.dumps({"limit": 20}).encode(), timeout_ms=8000)
                for g in json.loads(resp.data_json.decode()).get("goals", []):
                    title = g.get("title", "")
                    if g.get("status") == "needs_confirmation" and any(m in title for m in MARKERS):
                        print(f"auto-approving {g['id']} {title}", flush=True)
                        await client.send_action(
                            "goal_resume", json.dumps({"id": g["id"], "approve": True}).encode(), timeout_ms=15000
                        )
                await asyncio.sleep(20)
            try:
                client._writer.close()
                await client._writer.wait_closed()
            except Exception:
                pass
        except Exception as e:  # keep the loop alive; the kernel may be restarting
            print(f"auto-approver error: {e}", flush=True)
            await asyncio.sleep(20)


if __name__ == "__main__":
    asyncio.run(main())
