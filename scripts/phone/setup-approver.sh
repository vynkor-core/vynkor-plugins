#!/usr/bin/env bash
# Install the telegram-sweep auto-approver on the phone (run from the laptop).
#   scripts/phone/setup-approver.sh [ssh-host]
# Needs the aarch64 wheels in ~/.cache/vyn-phone-build/wheels (see docs runbook) and the python SDK
# checkout at ../vynkor-sdk-python relative to the repo's parent.
set -euo pipefail
HOST="${1:-mi6}"
HERE="$(cd "$(dirname "$0")" && pwd)"
WHEELS="${WHEELS:-$HOME/.cache/vyn-phone-build/wheels}"
SDK="${SDK:-$HERE/../../../vynkor-sdk-python/vynkor}"
[[ -d "$SDK" ]] || { echo "SDK not found: $SDK" >&2; exit 2; }

ssh "$HOST" 'mkdir -p ~/pylibs ~/.local/bin ~/.config/systemd/user'
for w in "$WHEELS"/*.whl; do
    scp -q "$w" "$HOST":/tmp/vyn-wheel.whl
    ssh "$HOST" 'cd ~/pylibs && python3 -c "import zipfile;zipfile.ZipFile(\"/tmp/vyn-wheel.whl\").extractall()" && rm -f /tmp/vyn-wheel.whl'
done
tar -C "$(dirname "$SDK")" --exclude=__pycache__ -cf - vynkor | ssh "$HOST" 'tar -C ~/pylibs -xf -'
scp -q "$HERE/vyn-auto-approver.py" "$HOST":.local/bin/vyn-auto-approver.py
scp -q "$HERE/vyn-auto-approve.service" "$HOST":.config/systemd/user/vyn-auto-approve.service
ssh "$HOST" 'chmod 755 ~/.local/bin/vyn-auto-approver.py &&
    PYTHONPATH=$HOME/pylibs python3 -c "import vynkor, google.protobuf, zstandard, websockets; print(\"sdk import ok\")" &&
    systemctl --user daemon-reload && systemctl --user enable --now vyn-auto-approve.service'
