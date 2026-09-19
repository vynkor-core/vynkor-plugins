#!/usr/bin/env python3
"""Every declared action needs a model-facing description and a risk label.

An action that registers with an empty description embeds as "name — " and
the agent plugin's embedding filter then drops it for any goal that does
not literally contain the tool name (see the agent plugin's
discovery.rs::description_from_parameters doc comment). Undocumented
actions are therefore not merely untidy — they are unreachable.

An action without an explicit `risk` falls through to the agent's
name-shaped inference (agent/src/tools.rs::infer_risk), which guesses from
the verb in the action name and cannot know that, say, overwriting a
secret is worse than overwriting a note. Inference is the fail-closed
backstop, not the intended source of truth.

Reports every offender, then fails. Exit 0 = clean.
"""
import glob
import json
import sys

# Plugins whose actions predate this rule. Shrink this list; never grow it.
GRANDFATHERED = {
    "automations",
    "email",
    "github",
    "metrics",
    "mic",
    "network",
    "scheduler",
    "search",
    "stt",
    "sync",
    "sync-client",
    "tts",
    "weather",
}

VALID_RISKS = {"low", "medium", "high", "critical"}


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
            name = f"{plugin_id}.{action.get('name', '<unnamed>')}"
            if not action.get("description", "").strip():
                offenders.append(f"{name}: no description")
            risk = action.get("risk", "").strip().lower()
            if risk not in VALID_RISKS:
                offenders.append(f"{name}: risk={action.get('risk') or '<absent>'!r}")

    print(f"check-action-docs: {checked} declared actions checked")
    if offenders:
        print(f"\n{len(offenders)} action(s) under-declared:", file=sys.stderr)
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
