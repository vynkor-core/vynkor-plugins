#!/usr/bin/env python3
"""Every declared action needs a model-facing description.

An action that registers with an empty description embeds as "name — " and
the agent plugin's embedding filter then drops it for any goal that does
not literally contain the tool name (see the agent plugin's
discovery.rs::description_from_parameters doc comment). Undocumented
actions are therefore not merely untidy — they are unreachable.

Reports every offender, then fails. Exit 0 = clean.
"""
import glob
import json
import sys

# Plugins whose actions predate this rule. Shrink this list; never grow it.
GRANDFATHERED = {
    "agent",
    "ai",
    "automations",
    "capture",
    "contacts",
    "daemon",
    "email",
    "github",
    "library",
    "metrics",
    "mic",
    "mqtt",
    "network",
    "rss",
    "scheduler",
    "search",
    "speech",
    "stt",
    "sync",
    "sync-client",
    "tts",
    "uptime",
    "vector-db",
    "weather",
}

def main():
    offenders = []
    checked = 0
    for path in sorted(glob.glob("plugins/*/plugin.json")):
        with open(path) as fh:
            manifest = json.load(fh)
        plugin_id = manifest.get("plugin_id", path.split("/")[1])
        if plugin_id in GRANDFATHERED:
            continue
        for action in manifest.get("actions", []):
            if not isinstance(action, dict):
                continue  # legacy string form: nothing to document yet
            checked += 1
            if not action.get("description", "").strip():
                offenders.append(f"{plugin_id}.{action.get('name', '<unnamed>')}")

    print(f"check-action-docs: {checked} declared actions checked")
    if offenders:
        print(f"\n{len(offenders)} action(s) missing a description:", file=sys.stderr)
        for name in offenders:
            print(f"  - {name}", file=sys.stderr)
        print(
            "\nAdd a one-line `description` to each action in its plugin.json. "
            "It is what the model reads to decide whether to call the tool.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
