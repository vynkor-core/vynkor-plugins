#!/usr/bin/env python3
"""Re-sign every published registry version over the S1 canonical message.

ROADMAP.md Sequencing #4 fix: entries packaged by pre-S1 package.sh carry
signatures over the three-field form, which vynm's verifier rejects. This
tool recomputes the seven-field signature from the stored fields — no
archives are rebuilt — after asserting every published zip still hashes to
its stored sha256. Nothing is written unless every entry signs cleanly.

Usage:
  VYNKOR_SIGNING_KEY_HEX=<64-hex seed> scripts/resign.py          # re-sign
  VYNKOR_SIGNING_KEY_FILE=<path> scripts/resign.py                # re-sign
  scripts/resign.py --check                                       # verify only

--check verifies existing signatures against the pinned maintainer public
key without any private key material.
"""

import hashlib
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import (
    Ed25519PrivateKey,
    Ed25519PublicKey,
)

REPO_ROOT = Path(__file__).resolve().parent.parent
REGISTRY_PATH = REPO_ROOT / "registry.json"
DIST_DIR = REPO_ROOT / "dist"
PINNED = "6ee352d706eaf5b5114a1252fb76bb8a2bfbf177b0e4c8e9c21f73b9019083ee"


def fail(msg: str) -> None:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def load_key() -> Ed25519PrivateKey:
    hex_seed = os.environ.get("VYNKOR_SIGNING_KEY_HEX", "")
    if not hex_seed:
        key_file = os.environ.get("VYNKOR_SIGNING_KEY_FILE", "")
        if not key_file:
            fail(
                "no signing key configured — set VYNKOR_SIGNING_KEY_HEX "
                "or VYNKOR_SIGNING_KEY_FILE (or run with --check)"
            )
        hex_seed = "".join(Path(key_file).read_text().split())
    hex_seed = "".join(hex_seed.split())
    try:
        seed = bytes.fromhex(hex_seed)
    except ValueError:
        fail("signing key is not valid hex")
    if len(seed) != 32:
        fail(f"signing key must be a 32-byte seed, got {len(seed)} bytes")
    return Ed25519PrivateKey.from_private_bytes(seed)


def entry_status(registry: dict, slug: str) -> str:
    # Mirrors vynm's flatten: root `revoked` wins, else the slug-level
    # status (versions carry no status field of their own).
    for r in registry.get("revoked", []):
        if r == slug or r.startswith(f"{slug}@"):
            return "revoked"
    return registry.get(slug, {}).get("status", "stable")


def signed_message(slug: str, version: str, ve: dict, status: str) -> bytes:
    # Byte-for-byte vynkor-manager/src/registry.rs::signed_message — any
    # divergence here silently reproduces the S1 breakage this fixes.
    fields = [
        slug,
        version,
        ve.get("sha256", ""),
        status,
        ve.get("archive_url", ""),
        ve.get("min_kernel_version", ""),
        ve.get("max_kernel_version", ""),
    ]
    return ":".join(fields).encode("ascii")


def collect_plan(registry: dict) -> list:
    plan = []
    slugs = sorted(k for k in registry if k not in ("meta", "revoked"))
    for slug in slugs:
        versions = registry[slug].get("versions", {})
        if not versions:
            fail(f"{slug}: no versions registered")
        for version in sorted(versions):
            ve = versions[version]
            # cache-busted urls carry "?v=" — the local zip keeps the bare name
            zip_name = Path(ve.get("archive_url", "").split("?")[0]).name
            if not zip_name:
                fail(f"{slug}@{version}: version entry has no archive_url")
            zip_path = DIST_DIR / slug / "versions" / version / zip_name
            if not zip_path.is_file():
                fail(f"{slug}@{version}: published zip missing at {zip_path}")
            plan.append((slug, version, ve, entry_status(registry, slug), zip_path))
    return plan


def assert_zips_byte_true(plan: list) -> None:
    for slug, version, ve, _, zip_path in plan:
        digest = hashlib.sha256(zip_path.read_bytes()).hexdigest()
        if digest != ve.get("sha256", ""):
            fail(
                f"{slug}@{version}: zip hash drifted from registry "
                f"({zip_path}) — archive was modified after packaging; "
                "refusing to sign"
            )


def main() -> None:
    check_only = "--check" in sys.argv[1:]
    try:
        registry = json.loads(REGISTRY_PATH.read_text())
    except (json.JSONDecodeError, OSError) as exc:
        fail(f"cannot read {REGISTRY_PATH}: {exc}")
    if not isinstance(registry, dict):
        fail("registry.json is not a map")

    plan = collect_plan(registry)
    assert_zips_byte_true(plan)

    pinned_key = Ed25519PublicKey.from_public_bytes(bytes.fromhex(PINNED))

    if check_only:
        bad, unsigned = [], []
        for slug, version, ve, status, _ in plan:
            msg = signed_message(slug, version, ve, status)
            sig_hex = ve.get("signature", "")
            if len(sig_hex) != 128:
                unsigned.append(f"{slug}@{version}")
                continue
            try:
                pinned_key.verify(bytes.fromhex(sig_hex), msg)
            except Exception:
                bad.append(f"{slug}@{version}")
        print(f"checked: {len(plan)} versions, "
              f"valid: {len(plan) - len(bad) - len(unsigned)}, "
              f"invalid: {bad}, unsigned: {unsigned}")
        sys.exit(1 if bad or unsigned else 0)

    private_key = load_key()
    derived_hex = private_key.public_key().public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw,
    ).hex()
    if derived_hex != PINNED:
        print(
            f"warning: signing public key {derived_hex} does not match the "
            f"pinned maintainer key {PINNED} — vynm will reject these "
            "entries until signed with the pinned key",
            file=sys.stderr,
        )

    results = []
    for slug, version, ve, status, _ in plan:
        msg = signed_message(slug, version, ve, status)
        sig = private_key.sign(msg)
        private_key.public_key().verify(sig, msg)
        results.append((slug, version, ve, sig.hex()))

    for slug, version, ve, sig_hex in results:
        ve["signature"] = sig_hex
        sig_path = DIST_DIR / slug / "versions" / version / "signature.sig"
        sig_path.write_text(sig_hex + "\n")

    meta = registry.setdefault("meta", {})
    meta["apiVersion"] = 2
    meta["lastUpdated"] = datetime.now(timezone.utc).strftime("%Y-%m-%d")

    ordered = {"meta": registry.get("meta", {}), "revoked": registry.get("revoked", [])}
    for slug in sorted(k for k in registry if k not in ("meta", "revoked")):
        ordered[slug] = registry[slug]
    REGISTRY_PATH.write_text(json.dumps(ordered, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")

    for slug, version, _, _ in results:
        print(f"  {slug}@{version}: re-signed")
    print(f"==> done: {len(results)} versions re-signed over the S1 form; "
          f"registry.json + signature.sig rewritten")


if __name__ == "__main__":
    main()
