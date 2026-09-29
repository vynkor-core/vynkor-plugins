# ai plugin

Provider-agnostic chat completion for Vynkor plugins. Exposes one action,
`chat_completion`. Doesn't open its own sockets — every request is routed
through the `network` plugin's `http_request` action, so `network` must
also be registered and running. See `ROADMAP.md` for the full design
rationale ("Decision: reuse `network`, don't reinvent").

v1 supports two providers: `anthropic` (Claude Messages API) and `openai`
(OpenAI-compatible chat completions — covers OpenAI, OpenRouter, and local
Ollama, since all three speak the same wire shape).

v0.3 adds `embedding` (OpenAI-compatible embeddings — `POST {base_url}/embeddings`):
covers OpenAI `text-embedding-3-small/large`, Voyage `voyage-3-lite`, и локальные
Ollama эмбеддинги (`nomic-embed-text` 768, `mxbai-embed-large` 1024, `all-minilm` 384).
Используется `vector-db`: когда туда передают `text`, он пересылает его в `ai embedding`
(модель берётся из Ollama), получает `embedding:[f32]` и сохраняет.

v0.1.1 adds **vision input** (`messages[].content` accepts image blocks),
**native tool-use passthrough** (`tools` in → `tool_calls` out), and
**provider-rate-limit retries** (`max_retries`/`retry_backoff_ms`, executed by
`network`'s `http_request` on HTTP 429/5xx).

**See [`USAGE.md`](./USAGE.md)** for the caller-facing guide: full
`chat_completion` request/response reference, per-provider examples, every
error message a caller can hit, and common patterns (multi-turn,
provider-agnostic calls, system prompts).

## Operator note

`ai` declares two kernel permissions — `network` and `secrets`
(`plugin.json`: `"permissions": ["network", "secrets"]`) — because it
invokes the `network` plugin's gated `http_request` action and the
`secrets` plugin's gated `secret_get` action, and the kernel's
anti-laundering check (T-19) requires callers of a gated action to hold
its permission too (Manifest v2: per-action `permission` on
`http_request`). It opens no sockets itself, so it's safe to run with
`sandbox: true`. `network` still needs `sandbox: false` (real egress) —
see `plugins/network/README.md`.

## Action: `chat_completion`

Request (`ActionRequest.params_json`):

```json
{
  "provider": "anthropic",
  "base_url": "https://api.anthropic.com",
  "model": "claude-sonnet-5",
  "api_key_env": "ANTHROPIC_API_KEY",
  "messages": [{"role": "user", "content": "..."}],
  "max_tokens": 1024,
  "timeout_ms": 30000
}
```

- `provider` — `"anthropic"` or `"openai"`. Omit it (with `base_url` and
  `api_key_env`) to name a model the host already knows — an entry from
  `list_models`, e.g. `{"model":"llama3.2:3b","messages":[...]}` — and the
  endpoint and key come from the model table; an id the host does not know
  is an `unknown model` error. Required for an ad-hoc model.
- `base_url` — required for `openai` (no safe default across
  OpenAI/OpenRouter/Ollama/self-hosted). Optional for `anthropic`, defaults
  to `https://api.anthropic.com`.
- `model` — required, non-empty.
- `api_key_env` — name under which the `ai` process resolves the key at
  call time, never a literal key. The caller never puts the raw key in
  the payload. Resolution is vault-first: `ai` asks the `secrets` plugin's
  vault for a secret stored under that exact name (`secret_set
  {"name":"...","value":"sk-..."}` by the operator), and falls back to the
  environment variable of the same name only when the vault has no
  non-empty value. The vault wins when both exist. Must appear in the
  operator's `AI_PLUGIN_ALLOWED_KEY_ENVS` allowlist (see "Configuration")
  — otherwise a caller could name *any* secret/env var the process has,
  not just a provider key, and exfiltrate it via a caller-controlled
  `base_url`. Not allowlisted, or unset in both sources → `ACTION_ERROR`;
  the key value never appears in any error string.
- `messages` — required, non-empty. Each message is `{role, content}` where
  `content` is either a plain string or an array of typed content blocks:
  - `{"type": "text", "text": "..."}`
  - `{"type": "image", "mime_type": "image/png|jpeg|gif|webp", "data_base64": "..."}` —
    max 8 images per message, 5 MiB decoded size each; validated before send.
- `tools` — optional array of native tool definitions passed through to the
  provider: `{name, description?, input_schema?}` (`input_schema` is a JSON
  Schema object, defaulting to an empty object schema; max 64 tools, 32 KiB
  schema each). The model's invocations come back as output `tool_calls`.
- `max_tokens` — optional, default `1024`, capped at `8192`.
- `timeout_ms` — optional, default and cap `30000`.
- `max_retries` / `retry_backoff_ms` — optional retry policy handed to
  `network`'s `http_request`, which re-sends on HTTP 429 and transient 5xx
  with doubling backoff (default `2` retries from `1000` ms; caps `5`/`5000`).

Response (`ActionResponse.data_json`) on success, normalized across both
providers:

```json
{
  "content": "...",
  "tool_calls": [{"id": "toolu_1", "name": "launch", "arguments_json": "{\"app_id\":\"firefox\"}"}],
  "stop_reason": "tool_use",
  "usage": {"input_tokens": 1, "output_tokens": 2}
}
```

`tool_calls` is present only when the model requested invocations
(anthropic `tool_use` blocks / openai `message.tool_calls`); plain-text
responses keep the exact pre-tools shape. `arguments_json` is the raw
arguments object serialized to a string — parse it against the tool's own
`input_schema`.

Errors → `ACTION_ERROR` with a human-readable message: malformed/missing
request fields, `api_key_env` not on the operator's allowlist or unset,
malformed provider JSON, non-2xx HTTP status from the provider, or any
error `network`'s `http_request` itself returns (SSRF block, timeout, DNS
failure, connection refused).

### Streaming (CD-03)

Same `params_json`, with `streaming: true` on the `ActionRequest`. `ai`
asks the provider for an SSE stream (via `network`'s streaming
`http_request`) and forwards it as a kernel streaming session:

1. `ActionResponse{ACTION_OK}`, `data_json` = `{"model": "..."}`, once the
   provider answered 2xx. Anything failing earlier (bad params, key, HTTP
   4xx/5xx with its body) is a plain `ACTION_ERROR` reply instead;
2. `ActionResponseChunk`s, each `{"type": "delta", "text": "..."}` — append
   them; they add up to the final `content`;
3. one chunk `{"type": "done", "result": {...}}` — the same object the
   buffered call returns (`content`, `tool_calls`, `stop_reason`, `usage`);
4. `SessionClose{reason: "done"}`.

An error mid-stream arrives as an `ACTION_ERROR` `ActionResponse`. Send
`SessionClose` to stop: `ai` closes its upstream session and `network`
drops the provider connection, so generation (and billing) stops. Usage is
recorded for completed streams only. Streaming never retries; the
`max_retries` param is ignored on this path. Only `chat_completion`
streams: a streaming request for any other action is rejected.

## Action: `embedding` (for vector-db)

Request:
```json
{
  "provider": "openai",
  "base_url": "http://localhost:11434/v1",
  "model": "nomic-embed-text",
  "api_key_env": "OLLAMA_API_KEY",
  "input": "hello world",
  "timeout_ms": 10000
}
```
- `provider` — only `"openai"` (covers OpenAI / Voyage / Ollama embeddings). `anthropic` → error.
- `input` — required, `1..10000` chars, single text (batch в будущем).
- `model` / `base_url` / `api_key_env` — как в `chat_completion`, резолвятся из `agent_id` или из БД `ai.db` если `model` там есть, иначе explicit.
- `timeout_ms` — default/cap `30000`.

Response:
```json
{ "embedding": [0.012, -0.03, ...], "dim": 768, "model": "nomic-embed-text", "usage": {"input_tokens":2,"output_tokens":0} }
```

Ошибки — те же что у `chat_completion`. См. `vector-db/README.md` — раздел «Архитектура эмбеддинга: Ollama → ai → vector-db».

Пример Ollama:
```bash
ollama pull nomic-embed-text  # 768 dim
ollama pull mxbai-embed-large  # 1024 dim
# в ai config: base_url http://localhost:11434/v1, api_key_env OLLAMA_API_KEY="" (пустой, но в allowlist)
# + network: NETWORK_PLUGIN_ALLOWED_HOSTS=localhost,127.0.0.1
```

## Configuration

`ai` reads no config file itself. The only configuration is environment
variables set in the kernel's `config.yaml`, under this plugin's `env:`
list — see `config.example.yaml` in this directory. Provider keys are
resolved vault-first: at call time `ai` asks the `secrets` plugin's vault
for the key under the `api_key_env` name, and only falls back to the
plugin's own environment variables when the vault has no non-empty value.
The vault wins when both exist — so the operator may store keys in the
vault instead of `env:` (via `secret_set {"name":"OPENAI_API_KEY","value":"sk-..."}`,
requires the `secrets` plugin to be registered), or keep using `env:` as
before.

`AI_PLUGIN_ALLOWED_KEY_ENVS` is **required**: a comma-separated,
exact-match allowlist of every env var name a caller's `api_key_env` may
reference. Default-deny — omit it and every `chat_completion` request is
rejected. Without this allowlist a caller could set `api_key_env` to any
env var the `ai` process happens to have (an unrelated secret, not just a
provider key) and have its value sent straight into an outbound request
header to a `base_url` the caller also controls.

```yaml
plugins:
  - id: ai
    binary: /opt/plugins/ai
    sandbox: true
    env:
      - AI_PLUGIN_ALLOWED_KEY_ENVS=ANTHROPIC_API_KEY,OPENAI_API_KEY
      - ANTHROPIC_API_KEY=sk-ant-...
      - OPENAI_API_KEY=sk-...
```

## Talking to a local model (Ollama)

Point `base_url` at Ollama's OpenAI-compatible endpoint and use `provider:
"openai"`:

```json
{
  "provider": "openai",
  "base_url": "http://localhost:11434/v1",
  "model": "deepseek-coder:1.3b",
  "api_key_env": "OLLAMA_API_KEY",
  "messages": [{"role": "user", "content": "hi"}]
}
```

Ollama needs no auth — pick any unset/empty variable name for
`api_key_env`, add it to `AI_PLUGIN_ALLOWED_KEY_ENVS` like any other (still
required even though the value itself is empty), and leave the var unset
or empty in `env:`; the `openai` adapter omits the `Authorization` header
entirely when the resolved key is empty.

This also requires `network`'s own config: its built-in SSRF blocklist
blocks loopback by default, so its `env:` needs

```yaml
- NETWORK_PLUGIN_ALLOWED_HOSTS=localhost,127.0.0.1
```

(see `plugins/network/config.example.yaml`) or every request to a local
model fails.

## Testing

`cargo test` — unit tests (72 as of 0.1.1) with no live network
(provider adapters are
tested against fixture JSON; `network`'s own tests cover the actual HTTP
send). End-to-end behavior (this README's examples, plus the SSRF
limitation above) was verified against a real kernel + `network` + `ai` +
local Ollama stack; there's no automated integration test for that yet.
