#!/usr/bin/env bash
# Build a release archive for a plugin and register it in registry.json (v2).
#
# This is the single tool that writes BOTH sides of the distribution store
# from one computation:
#   1. the hierarchical dist/ tree, and
#   2. the slug-keyed registry.json (v2 map) entry.
#
# dist/ layout written:
#   dist/<slug>/latest.json                 # {"version": "<latest registered>"}
#   dist/<slug>/assets/                     # (reserved for future asset hosting; not created here)
#   dist/<slug>/versions/<version>/
#       <slug>-<version>.zip                # binary + plugin.json (flat)
#       <slug>-<version>-src.zip            # plugin.json + src/ + Cargo.toml
#       plugin.json                         # browse copy of the manifest
#       checksum.sha256                     # "<sha256>  <slug>-<version>.zip" (two spaces)
#       signature.sig                       # Ed25519 sig over the S1 canonical seven-field message
#                                           #   "<slug>:<version>:<sha256>:<status>:<archive_url>:
#                                           #    <min_kernel_version>:<max_kernel_version>"
#                                           # (128 hex + newline); canonical form lives in
#                                           # vynkor-manager/src/registry.rs::signed_message
#
# registry.json (v2) shape written:
#   {
#     "meta":    { "apiVersion": 2, "lastUpdated": "<YYYY-MM-DD>" },
#     "revoked": [],
#     "<slug>": {
#       "name": ..., "description": ..., "category": ..., "tags": [...],
#       "status": ..., "source_url": ...,
#       "versions": {
#         "<version>": {
#           "archive_url": ..., "sha256": ..., "signature": ...,
#           "min_kernel_version": ..., "max_kernel_version": ...
#         }
#       }
#     }
#   }
#   Top-level key order is meta, revoked, then slugs alphabetically. A manifest
#   `requires` array (non-empty) adds a per-version "dependencies" map
#   (dep -> version range, default ">=0.0.0").
#
# Signing (optional):
#   VYNKOR_SIGNING_KEY_HEX   64-hex-char Ed25519 seed (32 bytes)
#   VYNKOR_SIGNING_KEY_FILE  path to a file containing that 64-hex-char seed
#   If either is set, the archive is signed over the ASCII message
#   "<slug>:<version>:<sha256>:<status>:<archive_url>:<min_kernel_version>:
#   <max_kernel_version>" (the S1 canonical form verified by
#   vynkor-manager's signed_message()), signature.sig is written, and the
#   registry entry's "signature" is set. The signature is self-verified
#   against the key that produced it (abort loudly on failure), and
#   cross-checked against the pinned maintainer public key (warn, do not
#   abort, when it differs). With no key configured, the entry gets an empty
#   signature (rejected by `vynm install` until signed) and no signature.sig
#   is written.
#
# Usage:
#   scripts/package.sh <plugin-dir-name> "<Display Name>" "<Description>" \
#       [--category <cat>] [--tags t1,t2] [--status <status>]
#
set -euo pipefail

positional=()
category="utility"
tags=""
status="stable"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --category)
            if [[ $# -lt 2 ]]; then echo "error: --category requires a value" >&2; exit 1; fi
            category="$2"; shift 2 ;;
        --tags)
            if [[ $# -lt 2 ]]; then echo "error: --tags requires a value" >&2; exit 1; fi
            tags="$2"; shift 2 ;;
        --status)
            if [[ $# -lt 2 ]]; then echo "error: --status requires a value" >&2; exit 1; fi
            status="$2"; shift 2 ;;
        -*)
            echo "error: unknown option: $1" >&2; exit 1 ;;
        *)
            positional+=("$1"); shift ;;
    esac
done

if [[ ${#positional[@]} -ne 3 ]]; then
    echo "usage: $0 <plugin-dir-name> <display-name> <description> [--category <cat>] [--tags t1,t2] [--status <status>]" >&2
    exit 1
fi

plugin_dir_name="${positional[0]}"
display_name="${positional[1]}"
description="${positional[2]}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
plugin_dir="$repo_root/plugins/$plugin_dir_name"
manifest="$plugin_dir/plugin.json"
dist_dir="$repo_root/dist"
registry="$repo_root/registry.json"

if [[ ! -f "$manifest" ]]; then
    echo "error: $manifest not found" >&2
    exit 1
fi

slug=$(jq -r '.plugin_id' "$manifest")
version=$(jq -r '.version' "$manifest")
binary=$(jq -r '.binary' "$manifest")
min_kernel=$(jq -r '.kernel_compatibility_range.min' "$manifest")
max_kernel=$(jq -r '.kernel_compatibility_range.max' "$manifest")
requires_json=$(jq -c '(.requires // [])' "$manifest")

# ---- Manifest v2 shape validation -------------------------------------------
# Fail fast, before any build work, on manifests that don't match the v2
# schema this script and the kernel consume. Legacy (pre-v2) manifests must
# be converted; each check below reports exactly what is wrong.

# `actions` must be an array of objects, each with a string `name`. Legacy
# v1 manifests listed plain strings — reject those with a pointer at the v2
# element shape so the maintainer knows what to convert to.
if ! jq -e '(.actions // []) | all(type == "object" and (.name | type == "string"))' "$manifest" >/dev/null; then
    echo "error: actions[0] must be an object {name, permission?, input?, output?} — Manifest v2" >&2
    exit 1
fi

# `config_schema` is optional, but must be a JSON object when present.
if ! jq -e '(.config_schema // {}) | type == "object"' "$manifest" >/dev/null; then
    echo "error: config_schema must be a JSON object — Manifest v2" >&2
    exit 1
fi

# `files` is optional, but must be an array of strings when present.
if ! jq -e '(.files // []) | type == "array" and all(type == "string")' "$manifest" >/dev/null; then
    echo "error: files must be an array of strings — Manifest v2" >&2
    exit 1
fi

echo "==> building release binary for $plugin_dir_name ($slug $version)"
cargo build --release --manifest-path "$plugin_dir/Cargo.toml"

bin_path="$plugin_dir/target/release/$binary"
if [[ ! -f "$bin_path" ]]; then
    echo "error: built binary not found at $bin_path" >&2
    exit 1
fi

version_dir="$dist_dir/$slug/versions/$version"
mkdir -p "$version_dir"

archive_name="$slug-$version.zip"
src_archive_name="$slug-$version-src.zip"
archive_path="$version_dir/$archive_name"
src_archive_path="$version_dir/$src_archive_name"

# ---- Manifest v2 `files` archive contents -----------------------------------
# When the manifest declares `files` (v2), the binary archive contains EXACTLY
# those files, resolved relative to the plugin dir: the entry matching
# `binary` maps to the just-built release binary, `plugin.json` to the
# manifest. Without `files` (legacy), the archive stays binary + plugin.json.
if jq -e 'has("files")' "$manifest" >/dev/null; then
    # The declared set must cover the two files the kernel always expects.
    if ! jq -e --arg b "$binary" '.files | index($b) != null and index("plugin.json") != null' "$manifest" >/dev/null; then
        echo "error: files must include the binary and plugin.json" >&2
        exit 1
    fi
    archive_files=()
    while IFS= read -r entry; do
        if [[ "$entry" == "$binary" ]]; then
            file_path="$bin_path"
        elif [[ "$entry" == "plugin.json" ]]; then
            file_path="$manifest"
        else
            file_path="$plugin_dir/$entry"
        fi
        if [[ ! -f "$file_path" ]]; then
            echo "error: files entry '$entry' not found in $plugin_dir" >&2
            exit 1
        fi
        archive_files+=("$file_path")
    done < <(jq -r '.files[]' "$manifest")
else
    archive_files=("$bin_path" "$manifest")
fi

echo "==> writing $archive_name"
rm -f "$archive_path"
zip -j -q "$archive_path" "${archive_files[@]}"

echo "==> writing $src_archive_name"
rm -f "$src_archive_path"
src_stage=$(mktemp -d)
trap 'rm -rf "$src_stage"' EXIT
src_root="$src_stage/$slug-src"
mkdir -p "$src_root"
cp "$manifest" "$src_root/"
cp -r "$plugin_dir/src" "$src_root/"
cp "$plugin_dir/Cargo.toml" "$src_root/"

# Vendor local path dependencies.
#
# There is no cargo workspace here, so a plugin that shares code does it with
# a relative path dependency (e.g. `vynkor-plugin-manifest = { path =
# "../_shared/plugin-manifest" }`). That path escapes the archive root: a
# consumer who unzips the src archive and runs `cargo build` gets
# "unable to update .../_shared/plugin-manifest: No such file or directory".
# The binary archive is unaffected — it ships an already-linked executable —
# so this failure only shows up for whoever tries to build from source, which
# is exactly the person least able to diagnose it.
#
# Copy each such crate under vendor/ and rewrite the path to point inside the
# archive, keeping the src archive a single self-contained root.
# Parse the dependency tables rather than grepping for `path =` — [lib] and
# [[bin]] carry their own `path` keys (`src/lib.rs`), and a grep picks those
# up and then fails claiming the plugin depends on its own source file.
mapfile -t path_deps < <(
  python3 - "$plugin_dir/Cargo.toml" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as fh:
    cargo = tomllib.load(fh)
tables = ["dependencies", "dev-dependencies", "build-dependencies"]
found = []
for table in tables:
    for spec in cargo.get(table, {}).values():
        if isinstance(spec, dict) and "path" in spec:
            found.append(spec["path"])
# [target.'cfg(...)'.dependencies] nests one level deeper.
for target in cargo.get("target", {}).values():
    for table in tables:
        for spec in target.get(table, {}).values():
            if isinstance(spec, dict) and "path" in spec:
                found.append(spec["path"])
print("\n".join(sorted(set(found))))
PY
)
for rel in "${path_deps[@]:-}"; do
  [ -n "$rel" ] || continue
  dep_src="$plugin_dir/$rel"
  if [ ! -d "$dep_src" ]; then
    echo "ERROR: $slug declares path dependency '$rel' but $dep_src does not exist" >&2
    exit 1
  fi
  dep_name=$(basename "$rel")
  mkdir -p "$src_root/vendor"
  # --exclude target/: the shared crate's build cache is not source and is
  # large enough to dominate the archive.
  rsync -a --exclude 'target/' "$dep_src/" "$src_root/vendor/$dep_name/"
  # Anchor on the exact quoted path so a second dependency whose path is a
  # prefix of this one cannot be rewritten by the wrong substitution.
  python3 - "$src_root/Cargo.toml" "$rel" "vendor/$dep_name" <<'PY'
import sys
path, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path).read()
needle = f'"{old}"'
if needle not in text:
    sys.exit(f"ERROR: could not rewrite path dependency {old!r} in {path}")
open(path, "w").write(text.replace(needle, f'"{new}"'))
PY
  echo "    vendored path dependency $rel -> vendor/$dep_name"
done

(cd "$src_stage" && zip -rq "$src_archive_path" "$slug-src")

echo "==> writing plugin.json browse copy"
cp "$manifest" "$version_dir/plugin.json"

sha256=$(sha256sum "$archive_path" | awk '{print $1}')
printf '%s  %s\n' "$sha256" "$archive_name" > "$version_dir/checksum.sha256"

# relative archive_url — vynm/kernel resolve it against the registry's own
# base URL, so host migration (github → R2 → custom domain) never breaks
# signatures (the S1 message covers archive_url as written)
archive_url="dist/$slug/versions/$version/$archive_name"
source_url="https://github.com/vynkor-core/vynkor-plugins/tree/main/plugins/$plugin_dir_name"

# ---- signing (optional) ----------------------------------------------------
seed=""
if [[ -n "${VYNKOR_SIGNING_KEY_HEX:-}" ]]; then
    seed="$VYNKOR_SIGNING_KEY_HEX"
elif [[ -n "${VYNKOR_SIGNING_KEY_FILE:-}" ]]; then
    seed="$(tr -d '[:space:]' < "$VYNKOR_SIGNING_KEY_FILE")"
fi

signature=""
if [[ -n "$seed" ]]; then
    echo "==> signing $slug:$version:$sha256"
    signature=$(python3 - "$seed" "$slug" "$version" "$sha256" \
        "$status" "$archive_url" "$min_kernel" "$max_kernel" <<'PYEOF'
import sys
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

(seed_hex, slug, version, sha256, status,
 archive_url, min_kernel, max_kernel) = sys.argv[1:9]
private_key = Ed25519PrivateKey.from_private_bytes(bytes.fromhex(seed_hex))
public_key = private_key.public_key()
message = (f"{slug}:{version}:{sha256}:{status}:{archive_url}:"
           f"{min_kernel}:{max_kernel}").encode("ascii")
sig = private_key.sign(message)

# Self-verify against the key that produced it: aborts loudly if the
# signature does not verify (catches a signing bug before it ships).
try:
    public_key.verify(sig, message)
except Exception as exc:
    print(f"error: signature self-verification failed: {exc}", file=sys.stderr)
    sys.exit(1)

# Cross-check against the pinned maintainer public key (the one compiled
# into vynkor-manager's official_source()). A mismatch means the configured
# seed is not the production signing key: warn loudly (do not abort, so key
# rotation / test keys are still usable), since `vynm install` will reject
# the entry until it is signed by the real key.
derived_hex = public_key.public_bytes(
    encoding=serialization.Encoding.Raw,
    format=serialization.PublicFormat.Raw,
).hex()
PINNED = "6ee352d706eaf5b5114a1252fb76bb8a2bfbf177b0e4c8e9c21f73b9019083ee"
if derived_hex != PINNED:
    print(
        f"warning: signing key public key {derived_hex} does not match the "
        f"pinned maintainer public key {PINNED}; the entry will be rejected "
        f"by `vynm install` until re-signed with the correct key",
        file=sys.stderr,
    )

print(sig.hex())
PYEOF
)
    printf '%s\n' "$signature" > "$version_dir/signature.sig"
else
    echo "warning: no signing key configured (VYNKOR_SIGNING_KEY_HEX / VYNKOR_SIGNING_KEY_FILE) — entry will have an empty signature; vynm install will reject it until signed" >&2
    rm -f "$version_dir/signature.sig"
fi

# ---- registry.json upsert ------------------------------------------------
echo "==> updating registry.json (slug=$slug)"
python3 - "$registry" "$slug" "$display_name" "$description" "$category" "$tags" \
    "$status" "$source_url" "$version" "$archive_url" "$sha256" "$signature" \
    "$min_kernel" "$max_kernel" "$requires_json" <<'PYEOF'
import datetime
import json
import sys

(registry_path, slug, name, description, category, tags_str, status,
 source_url, version, archive_url, sha256, signature,
 min_kernel, max_kernel, requires_json) = sys.argv[1:16]

requires = json.loads(requires_json) if requires_json else []

try:
    with open(registry_path) as f:
        registry = json.load(f)
except (FileNotFoundError, json.JSONDecodeError):
    registry = {}

if not isinstance(registry, dict):
    registry = {}

meta = registry.get("meta", {})
meta["apiVersion"] = 2
meta["lastUpdated"] = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
revoked = registry.get("revoked", [])

slug_entries = {k: v for k, v in registry.items() if k not in ("meta", "revoked")}

entry = slug_entries.get(slug, {})
entry["name"] = name
entry["description"] = description
entry["category"] = category
entry["tags"] = [t for t in tags_str.split(",") if t] if tags_str else []
entry["status"] = status
entry["source_url"] = source_url

versions = entry.get("versions", {})
version_entry = versions.get(version, {})
version_entry["archive_url"] = archive_url
version_entry["sha256"] = sha256
version_entry["signature"] = signature
version_entry["min_kernel_version"] = min_kernel
version_entry["max_kernel_version"] = max_kernel

if requires:
    dependencies = {dep: ">=0.0.0" for dep in requires}
    version_entry["dependencies"] = dependencies

versions[version] = version_entry
entry["versions"] = versions
slug_entries[slug] = entry

# Rebuild in canonical top-level key order: meta, revoked, slugs alphabetical.
out = {"meta": meta, "revoked": revoked}
for s in sorted(slug_entries):
    out[s] = slug_entries[s]

with open(registry_path, "w") as f:
    json.dump(out, f, indent=2)
    f.write("\n")
PYEOF

# ---- latest.json ----------------------------------------------------------
echo "==> updating dist/$slug/latest.json"
latest_version=$(python3 - "$registry" "$slug" <<'PYEOF'
import json
import re
import sys

registry_path, slug = sys.argv[1:3]
with open(registry_path) as f:
    reg = json.load(f)
versions = list(reg.get(slug, {}).get("versions", {}).keys())

def semver_key(v):
    m = re.match(r"^(\d+)\.(\d+)\.(\d+)$", v)
    return tuple(int(x) for x in m.groups()) if m else (0, 0, 0)

if not versions:
    print(f"error: no versions registered for slug {slug}", file=sys.stderr)
    sys.exit(1)

print(max(versions, key=semver_key))
PYEOF
)
printf '{\n  "version": "%s"\n}\n' "$latest_version" > "$dist_dir/$slug/latest.json"

if [[ -n "$signature" ]]; then
    signed="yes"
else
    signed="no"
fi
echo "==> done: $archive_path"
echo "    sha256:  $sha256"
echo "    signed:  $signed"
