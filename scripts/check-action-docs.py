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
# Empty as of the sweep that documented the last 13 — a new plugin has no
# excuse, so adding a name back here needs a reason in the commit message.
GRANDFATHERED: set[str] = set()

VALID_RISKS = {"low", "medium", "high", "critical"}

# A declared risk switches OFF the agent's inference for that action
# (agent/src/tools.rs::infer_missing_risk only fires on an undeclared risk),
# and inference is what auto-gates high/critical. So documenting an action
# that inference already gated, without restating the gate, silently REMOVES
# a confirmation the live system had. It has happened: declaring
# vec_delete/sync_del/schedule_delete high left all three ungated.
#
# Hence: high and critical must declare requires_confirmation, unless the
# action is listed here with a reason.
UNGATED_HIGH = {
    # Gating this would gate the voice assistant's every spoken question.
    # What it dispatches runs through the agent loop, which applies the
    # gates of whatever it actually calls.
    "daemon_ask": "entry point of the voice loop; the loop gates its own calls",
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
            name = f"{plugin_id}.{action.get('name', '<unnamed>')}"
            if not action.get("description", "").strip():
                offenders.append(f"{name}: no description")
            risk = action.get("risk", "").strip().lower()
            if risk not in VALID_RISKS:
                offenders.append(f"{name}: risk={action.get('risk') or '<absent>'!r}")
            action_name = action.get("name", "")
            if (risk in {"high", "critical"}
                    and not action.get("requires_confirmation")
                    and action_name not in UNGATED_HIGH):
                offenders.append(
                    f"{name}: risk={risk} without requires_confirmation "
                    "(inference used to gate this; declaring a risk turns that off)"
                )

    print(f"check-action-docs: {checked} declared actions checked")
    if offenders:
        print(f"\n{len(offenders)} action(s) under-declared:", file=sys.stderr)
        for name in offenders:
            print(f"  - {name}", file=sys.stderr)
        print(
            "\nEach action needs a one-line `description`, an explicit `risk` "
            f"({'/'.join(sorted(VALID_RISKS))}), and — at high or critical — "
            "`requires_confirmation`. The description is what the model reads "
            "to decide whether to call the tool; the risk and the gate decide "
            "whether a human sees it first.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
