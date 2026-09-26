#!/usr/bin/env bash
# smoke-voice.sh — end-to-end health check of the vynkor voice stack.
# Verifies every stage of the loop against the RUNNING kernel and prints a
# per-stage latency table. Exit 0 = all stages green.
#
# Usage: scripts/smoke-voice.sh [--no-llm] [--min-plugins N]
#   --no-llm      skip the chat_completion check (gateway flake shouldn't
#                 fail an otherwise-green local stack)
# Requires: curl, jq, python3; vyn-act (built on demand via cargo).
set -uo pipefail

CONFIG="${CONFIG:-$HOME/.config/vyn/config.yaml}"
MIN_PLUGINS=27
NO_LLM=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-llm) NO_LLM=1 ;;
        --min-plugins) MIN_PLUGINS="$2"; shift ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ACT="$HOME/.local/bin/vyn-act"
if [[ ! -x "$ACT" ]]; then
    echo "==> building vyn-act"
    (cd "$REPO/scripts/vyn-act" && cargo build --release) || exit 1
    install -m 0755 "$REPO/scripts/vyn-act/target/release/vyn-act" "$HOME/.local/bin/"
fi

CACHE="$HOME/.local/state/vyn/act-token"
if [[ -s "$CACHE" ]]; then
    TOK=$(cat "$CACHE")
else
    TOK=$(cd "$(dirname "$CONFIG")" && vyn token mint --device vyn-act --ttl-seconds 31536000 \
        --permissions "network,files_read,files_write,system,audio,notify,scheduler,browser,ipc_send,audio_stream,kernel_admin,event_publish,storage,secrets,clipboard,launch" | tail -1)
fi
# kernel >= 0.1.3: frame-MAC key is per plugin (HKDF of jwt_secret, plugin_id);
# the master jwt_secret itself is rejected with "frame MAC invalid"
JS=$(vyn token -c "$CONFIG" plugin-secret --plugin vyn-act)
export VYN_JWT_TOKEN="$TOK" VYN_JWT_SECRET="$JS"

FAILS=0
check() { # name expected_substr cmd...
    local name="$1" want="$2"; shift 2
    local out
    out=$("$@" 2>&1 | tail -1)
    if [[ "$out" == *"$want"* ]]; then
        printf '  \033[32mOK\033[0m   %s\n' "$name"
    else
        printf '  \033[31mFAIL\033[0m %s\n       got: %s\n' "$name" "${out:0:160}"
        FAILS=$((FAILS+1))
    fi
}

echo "==> kernel health (>= $MIN_PLUGINS plugins)"
HP=$(curl -sk -m 5 https://localhost:8888/health)
GOT=$(python3 -c "import json,sys;print(json.loads(sys.argv[1]).get('plugins',0))" "$HP" 2>/dev/null || echo 0)
if [[ "$GOT" -ge "$MIN_PLUGINS" ]]; then
    printf '  \033[32mOK\033[0m   %s plugins registered\n' "$GOT"
else
    printf '  \033[31mFAIL\033[0m only %s/%s plugins\n' "$GOT" "$MIN_PLUGINS"
    FAILS=$((FAILS+1))
fi

# The kernel drops a disconnected plugin's registration asynchronously, so a
# vyn-act started right after the previous one exits can hit "plugin already
# registered: vyn-act" for a few hundred ms. Retry that one error only.
run() {
    local out err i
    err=$(mktemp)
    for i in 1 2 3 4 5 6; do
        out=$(timeout 60 vyn-act "$@" 2>"$err")
        [[ "$out$(<"$err")" == *"plugin already registered"* ]] || break
        sleep 0.3
    done
    cat "$err" >&2
    rm -f "$err"
    printf '%s\n' "$out"
}

echo "==> system plugin (routing sanity)"
check "sys_info" '"hostname"' run sys_info '{}'

echo "==> ai profiles"
AG=$(run list_agents '{}' 20000)
if [[ "$AG" == *'"default"'* ]]; then
    printf '  \033[32mOK\033[0m   default profile present\n'
else
    printf '  \033[31mFAIL\033[0m no default agent profile\n'; FAILS=$((FAILS+1))
fi

if [[ "$NO_LLM" -eq 0 ]]; then
    echo "==> LLM (default profile — cloud latency varies)"
    T0=$(date +%s%N)
    ANS=$(run chat_completion '{"agent_id":"default","messages":[{"role":"user","content":"Ответь одним словом: ок"}]}' 45000)
    T1=$(date +%s%N)
    if [[ "$ANS" == *"content"* ]]; then
        printf '  \033[32mOK\033[0m   chat_completion %s ms\n' "$(( (T1-T0)/1000000 ))"
    else
        printf '  \033[33mSKIP\033[0m LLM недоступен (%s ms): %s\n' "$(( (T1-T0)/1000000 ))" "$(echo "$ANS" | head -c 100)"
    fi
else
    echo "==> LLM skipped (--no-llm)"
fi

echo "==> voice loop: synth -> transcribe -> play"
T0=$(date +%s%N); SYN=$(run tts_synthesize '{"provider":"sherpa","text":"Проверка распознавания речи один два три","voice":"sid:0","format":"wav"}' 85000); T1=$(date +%s%N)
if [[ "$SYN" == *"audio_base64"* ]]; then
    printf '  \033[32mOK\033[0m   tts_synthesize %s ms\n' "$(( (T1-T0)/1000000 ))"
    python3 -c "
import json, base64
d = json.load(open('/dev/stdin'))
open('/tmp/vyn-smoke.wav','wb').write(base64.b64decode(d['audio_base64']))
json.dump({'provider':'sherpa','format':'wav','language':'ru',
           'audio_base64': d['audio_base64']}, open('/tmp/vyn-smoke-params.json','w'))
" <<< "$SYN"
    T0=$(date +%s%N); TR=$(run stt_transcribe @/tmp/vyn-smoke-params.json 85000); T1=$(date +%s%N)
    if [[ "$TR" == *"два"* ]]; then
        printf '  \033[32mOK\033[0m   stt_transcribe roundtrip %s ms: %s\n' \
            "$(( (T1-T0)/1000000 ))" "$(python3 -c "import json,sys;print(json.load(sys.stdin).get('text','')[:60])" <<< "$TR")"
    else
        printf '  \033[31mFAIL\033[0m stt_transcribe: %s\n' "${TR:0:120}"; FAILS=$((FAILS+1))
    fi
else
    printf '  \033[31mFAIL\033[0m tts_synthesize: %s\n' "${SYN:0:120}"; FAILS=$((FAILS+1))
fi
OUT=$(run sound_status '{}' 15000 2>&1 | tail -1)
if [[ "$OUT" == *"count"* ]]; then
    printf '  \033[32mOK\033[0m   sound_status\n'
else
    printf '  \033[31mFAIL\033[0m sound_status: %s\n' "${OUT:0:120}"; FAILS=$((FAILS+1))
fi

echo
if [[ "$FAILS" -eq 0 ]]; then
    echo "SMOKE PASS"
    exit 0
fi
echo "SMOKE FAIL: $FAILS stage(s) down"
exit 1
