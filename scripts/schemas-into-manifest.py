#!/usr/bin/env python3
"""Copy JSON-Schema params from an agent tools file into a plugin manifest.

The agent plugin's operator tools file (AGENT_PLUGIN_TOOLS_FILE) holds
hand-written `parameters` schemas that duplicate what the owning plugin
should declare. This lifts them into the plugin's own plugin.json as
`input`, so the kernel-manifest catalog layer becomes complete and the
operator file entry can be deleted.

Only actions the manifest already declares are touched, and an existing
`input` is never overwritten — the plugin stays the authority.

    python3 scripts/schemas-into-manifest.py \\
        --tools-file ~/.config/vyn/agent-tools.json \\
        --manifest plugins/telegram/plugin.json --dry-run
"""
import argparse
import json
import sys


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tools-file", required=True)
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    with open(args.tools_file) as fh:
        schemas = {
            t["name"]: t["parameters"]
            for t in json.load(fh).get("tools", [])
            if t.get("name") and t.get("parameters")
        }
    with open(args.manifest) as fh:
        manifest = json.load(fh)

    filled, skipped_present, skipped_absent = [], [], []
    for action in manifest.get("actions", []):
        name = action.get("name")
        if name not in schemas:
            skipped_absent.append(name)
        elif "input" in action:
            skipped_present.append(name)
        else:
            action["input"] = schemas[name]
            filled.append(name)

    print(f"fill:            {len(filled)} {sorted(filled)}")
    print(f"already had input: {len(skipped_present)} {sorted(skipped_present)}")
    print(f"no schema in tools file: {len(skipped_absent)} {sorted(skipped_absent)}")

    if args.dry_run:
        print("dry run — manifest not written")
        return 0
    with open(args.manifest, "w") as fh:
        json.dump(manifest, fh, indent=2, ensure_ascii=False)
        fh.write("\n")
    print(f"wrote {args.manifest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
