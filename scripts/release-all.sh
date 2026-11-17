#!/usr/bin/env bash
# Package (and sign, when a key is set) plugins at their manifest versions.
# Display metadata comes from scripts/registry-meta.json; each run upserts the
# plugin's entry in registry.json and writes dist/<slug>/ (see package.sh).
#
# Usage:
#   scripts/release-all.sh                # every plugin except SKIP
#   scripts/release-all.sh agent ai       # just these
#
# Signing: VYNKOR_SIGNING_KEY_HEX or VYNKOR_SIGNING_KEY_FILE, as package.sh.
# Without either the archives are built unsigned: a dry run, `vynm install`
# and the app's store refuse unsigned entries.
#
# Build on an old glibc (CI uses ubuntu-22.04): a binary links against the
# glibc it was built with and will not start on an older one.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
META="$REPO/scripts/registry-meta.json"

# phone: aarch64 musl for the phone only (scripts/phone-build.sh)
SKIP=(phone)

if [[ $# -gt 0 ]]; then
    slugs=("$@")
else
    slugs=()
    for d in "$REPO"/plugins/*/; do
        s="$(basename "$d")"
        [[ -f "$d/plugin.json" ]] || continue
        [[ " ${SKIP[*]} " == *" $s "* ]] && continue
        slugs+=("$s")
    done
fi

failed=()
for s in "${slugs[@]}"; do
    if [[ "$(jq -r --arg s "$s" 'has($s)' "$META")" != "true" ]]; then
        echo "error: $s has no entry in scripts/registry-meta.json" >&2
        failed+=("$s")
        continue
    fi
    # \x1f, not tab: tab is IFS whitespace, so an empty field would collapse
    IFS=$'\x1f' read -r name desc category tags status < <(jq -r --arg s "$s" \
        '.[$s] | [.name, .description, .category, (.tags | join(",")), .status] | join("\u001f")' "$META")
    echo "════ $s ════"
    args=("$s" "$name" "$desc" --category "$category" --status "$status")
    [[ -n "$tags" ]] && args+=(--tags "$tags")
    if ! "$REPO/scripts/package.sh" "${args[@]}"; then
        echo "::error::packaging $s failed"
        failed+=("$s")
    fi
done

if [[ ${#failed[@]} -gt 0 ]]; then
    echo "failed: ${failed[*]}" >&2
    exit 1
fi
echo "packaged ${#slugs[@]} plugins"
