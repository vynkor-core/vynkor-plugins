#!/usr/bin/env python3
"""Cross-check each plugin's config_schema against the env vars its code reads.

Two failure classes, both of which have shipped before:
  * schema-but-unread — the manifest (and so the web settings form)
    advertises a knob the code never reads (weather, mqtt);
  * read-but-undocumented — the code reads a <PREFIX>_PLUGIN_* var that the
    schema doesn't mention, so operators can't discover it.

Only string literals of the form "<X>_PLUGIN_<Y>" are seen; names built with
format!() (e.g. per-account TELEGRAM_PLUGIN_API_ID_<ID>) are covered by the
schema's patternProperties and skipped here.

    python3 scripts/check-config-schema.py
"""
import json
import os
import re
import sys

ENV_LITERAL = re.compile(r'"([A-Z][A-Z0-9_]*_PLUGIN_[A-Z0-9_]+)"')


def rust_sources(plugin_dir):
    text = []
    for root, dirs, files in os.walk(plugin_dir):
        dirs[:] = [d for d in dirs if d != "target"]
        for name in files:
            if name.endswith(".rs"):
                with open(os.path.join(root, name), errors="ignore") as fh:
                    text.append(fh.read())
    return "\n".join(text)


def main():
    failures = []
    checked = 0
    for slug in sorted(os.listdir("plugins")):
        manifest_path = os.path.join("plugins", slug, "plugin.json")
        if not os.path.isfile(manifest_path):
            continue
        with open(manifest_path) as fh:
            schema = json.load(fh).get("config_schema")
        if schema is None:
            failures.append(f"{slug}: no config_schema")
            continue
        declared = set(schema.get("properties", {}))
        patterns = [re.compile(p) for p in schema.get("patternProperties", {})]
        src = rust_sources(os.path.join("plugins", slug))
        read = set(ENV_LITERAL.findall(src))
        for name in sorted(declared):
            if name not in src:
                failures.append(f"{slug}: {name} is in config_schema but never read")
        for name in sorted(read - declared):
            if name.endswith("_") or any(p.match(name) for p in patterns):
                continue
            failures.append(f"{slug}: {name} is read but missing from config_schema")
        checked += 1
    for line in failures:
        print(line)
    print(f"check-config-schema: {checked} plugins, {len(failures)} problem(s)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
