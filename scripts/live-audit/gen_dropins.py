"""Generate plugins.d drop-ins + per-plugin JWTs (kernel requires sub==plugin_id tokens on secured kernels)."""
import os, json, time, hmac, hashlib, base64

JWT_SECRET = "audit-test-secret-0123456789abcdef0123456789abcdef"
PLUGINS = "/home/behzod/projects/vynkor-core/vynkor-plugins/plugins"
OUT = "/tmp/opencode/audit/plugins.d"
DATA = "/tmp/opencode/audit/data"
MODELS = "/home/behzod/projects/vynkor-core/vynkor-plugins/models"


def b64u(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def mint(plugin_id, permissions, ipc_targets, ttl=86400 * 7):
    header = {"alg": "HS256", "typ": "JWT"}
    now = int(time.time())
    claims = {"sub": plugin_id, "permissions": permissions,
              "ipc_targets": ipc_targets, "exp": now + ttl, "iat": now}
    hj, cj = b64u(json.dumps(header).encode()), b64u(json.dumps(claims).encode())
    sig = hmac.new(JWT_SECRET.encode(), f"{hj}.{cj}".encode(), hashlib.sha256).digest()
    return f"{hj}.{cj}.{b64u(sig)}"

# slug -> (binary_relpath, [env KEY=VAL...])
PLUGINS_ENV = {
    "ping-pong-rs": ("target/release/ping-pong-rs", []),
    "network":      ("target/release/network", []),
    "ai":           ("target/release/ai", [
        "AI_PLUGIN_ALLOWED_KEY_ENVS=ANTHROPIC_API_KEY",
    ]),
    "database":     ("target/release/database", [
        f"DATABASE_PLUGIN_DATA_DIR={DATA}/database",
    ]),
    "tts":          ("target/release/tts", [
        f"TTS_PLUGIN_LOCAL_MODEL_DIR={MODELS}/tts/piper-ru_RU-denis-medium",
        "TTS_PLUGIN_LOCAL_MODEL_TYPE=piper",
        "TTS_PLUGIN_ALLOWED_KEY_ENVS=OPENAI_API_KEY",
    ]),
    "stt":          ("target/release/stt", [
        f"STT_PLUGIN_LOCAL_MODEL_DIR={MODELS}/stt/zipformer-ru-int8",
        "STT_PLUGIN_LOCAL_MODEL_TYPE=transducer",
        "STT_PLUGIN_ALLOWED_KEY_ENVS=OPENAI_API_KEY",
    ]),
    "secrets":      ("target/release/secrets", [
        "SECRETS_PLUGIN_MASTER_KEY=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        f"SECRETS_PLUGIN_DATA_DIR={DATA}/secrets",
    ]),
    "gated-write":  ("target/release/gated-write", [
        f"GATED_WRITE_DATA_DIR={DATA}/gated-write",
    ]),
    "notify":       ("target/release/notify", [
        f"NOTIFY_PLUGIN_DATA_DIR={DATA}/notify",
    ]),
    "sync":         ("target/release/sync", [
        f"SYNC_PLUGIN_DATA_DIR={DATA}/sync",
    ]),
    "sync-client":  ("target/release/sync-client", []),
    "notes":        ("target/release/notes", []),
    "calendar":     ("target/release/calendar", []),
    "media":        ("target/release/media", []),
    "clipboard":    ("target/release/clipboard", []),
    "filesystem":   ("target/release/filesystem", [
        "FILES_PLUGIN_ALLOWED_ROOTS=/tmp",
    ]),
    "search":       ("target/release/search", [
        "SEARCH_PLUGIN_ALLOWED_KEY_ENVS=BRAVE_API_KEY,TAVILY_API_KEY",
    ]),
    "system":       ("target/release/system", []),
}

TEMPLATE = """\
id: {slug}
binary: {binary}
restart: on-failure
max_restarts: 3
sandbox: false
{envs}"""

for slug, (binrel, envs) in PLUGINS_ENV.items():
    binary = os.path.join(PLUGINS, slug, binrel)
    assert os.path.exists(binary), f"missing binary for {slug}: {binary}"
    man = json.load(open(os.path.join(PLUGINS, slug, "plugin.json")))
    slug_id = man["plugin_id"]  # the id the binary registers with — token sub must match it
    token = mint(slug_id, man.get("permissions", []), man.get("ipc_targets", []))
    # No VYN_JWT_SECRET: the supervisor injects the per-plugin frame-MAC key
    # (kernel >= 0.1.3) and ignores a configured one. Never ship the master.
    envs = list(envs) + ["VYN_JWT_TOKEN=" + token]
    env_block = ""
    if envs:
        env_block = "env:\n" + "".join(f"  - \"{e}\"\n" for e in envs)
    with open(os.path.join(OUT, f"{slug}.yaml"), "w") as f:
        f.write(TEMPLATE.format(slug=slug_id, binary=binary, envs=env_block))
    print("wrote", slug)
