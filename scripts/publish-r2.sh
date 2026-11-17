#!/usr/bin/env bash
# Publish registry.json + dist/ to Cloudflare R2 over the S3 API (rclone).
#
# Env (from .env or environment):
#   R2_BUCKET          bucket name (default vynkor-plugins)
#   R2_ACCOUNT_ID      cloudflare account id -> endpoint
#   R2_ACCESS_KEY_ID   r2 token access key
#   R2_SECRET_ACCESS_KEY  r2 token secret
#   PUBLIC_BASE_URL    optional; when set, every registered archive_url is
#                      fetched and sha256-verified after upload
# Usage:
#   scripts/publish-r2.sh [--dry-run] [--wipe]
#
# --wipe deletes every archive under dist/ in the bucket first: a clean store
# where only what this dist/ holds (and registry.json lists) is left.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ -f "$REPO_ROOT/.env" ]] && set -a && source "$REPO_ROOT/.env" && set +a

: "${R2_BUCKET:=vynkor-plugins}"
: "${R2_ACCOUNT_ID:?set R2_ACCOUNT_ID}"
: "${R2_ACCESS_KEY_ID:?set R2_ACCESS_KEY_ID}"
: "${R2_SECRET_ACCESS_KEY:?set R2_SECRET_ACCESS_KEY}"

DRY=""
WIPE=""
for a in "$@"; do
    case "$a" in
        --dry-run) DRY="--dry-run" ;;
        --wipe) WIPE=1 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

export RCLONE_CONFIG_R2_TYPE=s3
export RCLONE_CONFIG_R2_PROVIDER=Cloudflare
export RCLONE_CONFIG_R2_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID"
export RCLONE_CONFIG_R2_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY"
export RCLONE_CONFIG_R2_ENDPOINT="https://$R2_ACCOUNT_ID.r2.cloudflarestorage.com"
# upload-scoped tokens can't CreateBucket — skip rclone's bucket probe
export RCLONE_CONFIG_R2_NO_CHECK_BUCKET=true

rclone_copy() {
    rclone copyto "$1" "R2:$R2_BUCKET/$2" \
        --header "Cache-Control: $3" $DRY
}

if [[ -n "$WIPE" ]]; then
    echo "==> deleting old archives"
    rclone purge "R2:$R2_BUCKET/dist" $DRY || echo "    (nothing to delete)"
fi

# 1. archives first — a registry entry must never point at a missing object;
#    content-addressed by slug+version+sha256, so immutable caching is safe
echo "==> uploading archives"
find "$REPO_ROOT/dist" -name '*.zip' -print0 | while IFS= read -r -d '' z; do
    rel="${z#$REPO_ROOT/}"
    rclone_copy "$z" "$rel" "public, max-age=31536000, immutable"
done

# 2. sidecars (checksum.sha256 / signature.sig / latest.json / plugin.json)
echo "==> uploading sidecar metadata"
find "$REPO_ROOT/dist" \( -name 'checksum.sha256' -o -name 'signature.sig' \
    -o -name 'latest.json' -o -name plugin.json \) -print0 | while IFS= read -r -d '' f; do
    rel="${f#$REPO_ROOT/}"
    rclone_copy "$f" "$rel" "public, max-age=3600"
done

# 3. registry LAST, short cache so revocations propagate quickly
echo "==> uploading registry.json"
rclone_copy "$REPO_ROOT/registry.json" "registry.json" "public, max-age=300"

echo "==> published to s3://$R2_BUCKET ($(rclone size "R2:$R2_BUCKET" | tail -1))"

# 4. self-check: every archive_url must serve bytes matching its sha256
if [[ -n "${PUBLIC_BASE_URL:-}" ]]; then
    echo "==> verifying served bytes against registry sha256"
    python3 - "$REPO_ROOT" "$PUBLIC_BASE_URL" <<'PYEOF'
import hashlib, json, sys, urllib.request
root, base = sys.argv[1], sys.argv[2].rstrip("/")
# cloudflare 403s the default Python-urllib user agent on r2.dev
UA = {"User-Agent": "vynkor-publish/1.0"}

def fetch(url):
    # r2.dev occasionally truncates mid-body; retry until complete
    for attempt in range(5):
        try:
            return urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=60).read()
        except Exception:
            if attempt == 4:
                raise

reg = json.loads(fetch(base + "/registry.json"))
fails = checked = 0
for slug, entry in reg.items():
    if slug in ("meta", "revoked"):
        continue
    for ver, ve in sorted(entry.get("versions", {}).items()):
        url = f"{base}/{ve['archive_url']}"
        digest = hashlib.sha256(fetch(url)).hexdigest()
        ok = digest == ve["sha256"]
        print(f"  {'OK ' if ok else 'BAD'} {slug}@{ver}")
        checked += 1
        fails += not ok
print(f"checked {checked} versions, {fails} failures")
sys.exit(1 if fails or not checked else 0)
PYEOF
fi
