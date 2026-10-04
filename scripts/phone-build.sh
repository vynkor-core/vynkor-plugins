#!/usr/bin/env bash
# Cross-build plugins for the phone (aarch64, static musl) and stage them for deployment.
#
#   scripts/phone-build.sh network database ai ...      # build these
#   OUT=/some/dir scripts/phone-build.sh ai              # stage elsewhere (default: target/phone-dist)
#
# Needs: rustup target aarch64-unknown-linux-musl, and a zig compiler on PATH or via
# `python3 -m ziglang` (pip install ziglang in a venv, then ZIG="/path/venv/bin/python -m ziglang").
# Why zig: vynkor-sdk pulls zstd-sys (C) and some plugins bundle sqlite (C); there is no
# aarch64 gcc here. `zig cc` compiles the C, `rust-lld` links (zig cc as a linker chokes on
# --fix-cortex-a53-843419).
#
# Stages, per plugin: $OUT/<plugin>/<binary> and $OUT/<plugin>/plugin.json.
set -euo pipefail

cd "$(dirname "$0")/.."
ZIG="${ZIG:-python3 -m ziglang}"
OUT="${OUT:-$PWD/target/phone-dist}"
TARGET=aarch64-unknown-linux-musl
[[ $# -gt 0 ]] || { echo "usage: $0 <plugin>..." >&2; exit 2; }

WRAP="$(mktemp -d)"
trap 'rm -rf "$WRAP"' EXIT
cat >"$WRAP/zcc" <<EOF
#!/bin/sh
# cc-rs passes --target=...; zig wants -target, and picks it from us
args=""
for a in "\$@"; do case "\$a" in --target=*) ;; *) args="\$args \$a";; esac; done
exec $ZIG cc -target aarch64-linux-musl \$args
EOF
printf '#!/bin/sh\nexec %s ar "$@"\n' "$ZIG" >"$WRAP/zar"
chmod +x "$WRAP/zcc" "$WRAP/zar"

export CC_aarch64_unknown_linux_musl="$WRAP/zcc"
export AR_aarch64_unknown_linux_musl="$WRAP/zar"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="${RUSTFLAGS:-} -C linker-flavor=ld.lld"

for p in "$@"; do
    manifest="plugins/$p/Cargo.toml"
    [[ -f "$manifest" ]] || { echo "no such plugin: $p" >&2; exit 2; }
    bin="$(awk '/^\[\[bin\]\]/{f=1} f&&/^name/{gsub(/[" ]/,"",$3); print $3; exit}' "$manifest")"
    echo "==> $p (bin $bin)"
    cargo build --release --target "$TARGET" --manifest-path "$manifest"
    tdir="$(cargo metadata --manifest-path "$manifest" --format-version 1 --no-deps |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
    mkdir -p "$OUT/$p"
    install -m 755 "$tdir/$TARGET/release/$bin" "$OUT/$p/$bin"
    install -m 644 "plugins/$p/plugin.json" "$OUT/$p/plugin.json"
    file "$OUT/$p/$bin" | cut -c1-140
done
