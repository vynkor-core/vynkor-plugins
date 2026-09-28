# network plugin

Outbound HTTP for Vynkor plugins/kernel. Exposes two actions:
`http_request` (guarded by `PERMISSION_NETWORK`) and `network_stats`
(per-caller request/error/latency counters). See
`docs/superpowers/specs/2026-07-05-network-plugin-design.md` for the full
design (request/response shape, guardrails, error mapping).

v1 is HTTP only — no WebSocket.

## Operator note

This plugin needs real network egress. In the kernel's `config.yaml`
`plugins:` entry for `network`, set `sandbox: false`. `sandbox: true` puts
the plugin in an isolated PID+net namespace with no route out
(`src/plugins/runner.rs`), which makes every `http_request` fail.

## Extra SSRF blocklist (operator-configurable)

Besides the built-in blocklist (loopback, RFC1918 private ranges,
link-local, multicast, broadcast, cloud metadata `169.254.169.254`), an
operator can block additional IPs and/or hostnames via the
`NETWORK_PLUGIN_EXTRA_BLOCKED_HOSTS` environment variable: a comma-separated
list, each entry either a literal IP address or a bare hostname (matched
case-insensitively against the request's host before DNS resolution).

Set it via the plugin's `env:` list in the kernel's `config.yaml` — see
`config.example.yaml` in this directory for a full example entry. Example:

```yaml
env:
  - NETWORK_PLUGIN_EXTRA_BLOCKED_HOSTS=10.99.0.5,internal-admin.corp,203.0.113.7
```

Both forms are enforced at the DNS resolver used for every connection
(initial request and any redirect hop) — see `src/handler.rs`'s
`SsrfSafeResolver` and `src/ssrf.rs`'s `Blocklist`.

## Retries

`http_request` accepts optional `max_retries` (default `0`, capped at `5`)
and `retry_backoff_ms` (default `200`, capped at `5000`, doubling each
attempt). A response is retried only on `429` or `5xx`; any other status
(including other `4xx`) is returned on the first attempt. Transport-level
failures (connection refused, timeout) are always retried up to
`max_retries`. Retries are opt-in — callers get none unless they ask.

Deterministic failures are never retried, no matter what `max_retries`
says: an SSRF-policy rejection (blocked host/IP — both literal-IP and
hostname hosts are rejected before any attempt is made) and a response
body over the 10 MiB cap fail on the first attempt, since retrying
reproduces them exactly. A redirect hop rejected by the SSRF-gated policy
is likewise not retried.

Retries aren't restricted to idempotent methods; a caller requesting
retries on e.g. `POST` is responsible for that being safe for its endpoint.

## Proxy (operator-only)

By default no HTTP proxy is used, and ambient `HTTP_PROXY`/`HTTPS_PROXY`
environment variables are ignored — `reqwest` honors them by default, which
would otherwise let a request bypass `SsrfSafeResolver` entirely (the
target host gets resolved by the proxy, not by this plugin).

An operator can opt in via `NETWORK_PLUGIN_PROXY_URL` in the plugin's `env:`
list, e.g. `NETWORK_PLUGIN_PROXY_URL=http://proxy.internal:8080`. This is
deliberately not a per-request param: once set, the SSRF blocklist no
longer covers hosts reached through that proxy — only enable it pointed at
a proxy you trust to do its own filtering.

## Logging

Every attempt (including retries) logs one JSON line to stdout: `plugin`,
`method`, `host`, `attempt`, `status`, `error`, `duration_ms`.

## Events

Every `http_request` — success or failure — also publishes a
`plugin.network.request_completed` event to the kernel event bus (the
kernel prepends the `plugin.<sender_id>.` namespace; the bus matches
subscriptions exactly, so subscribe to `plugin.network.request_completed`).
The payload is one JSON object:

```json
{
  "status": 200,
  "host": "example.com",
  "latency_ms": 42,
  "retry_count": 0,
  "error": ""
}
```

`status` is the final attempt's HTTP status, or `0` when no HTTP response
was obtained (SSRF-policy rejection, transport failure). `retry_count` is
`attempts - 1` — how many times the request was actually retried, so a
subscriber can watch retry storms. `error` is the failure message, empty on
success.

Publishing is best-effort and fire-and-forget: the `ActionResponse` is
always sent first, and a dropped event (loop shutting down, channel full)
never delays or fails the caller's reply. Requires `PERMISSION_EVENT_PUBLISH`.

## Streaming responses (CD-03)

Send the `ActionRequest` with `streaming: true` (same `params_json`) to get
the body as it arrives instead of one buffered reply — what `ai` uses for
token streaming:

1. `ActionResponse{ACTION_OK}` once the response head is in; `data_json` is
   `{"status": <u16>, "headers": {...}}` (any HTTP status, errors included);
2. `ActionResponseChunk`s with the raw body bytes, `seq` from 0;
3. `SessionClose{reason: "done"}` at end of body.

A failure at any point is an `ACTION_ERROR` `ActionResponse`. Send
`SessionClose` to stop: `network` drops the upstream connection, so the
remote stops sending (and billing). `timeout_ms` bounds the wait for the
head and each gap between chunks, not the whole transfer. No retries, cache
or cookie jar on this path; the 10 MiB body cap and the per-caller in-flight
cap still apply.

## Response body encoding

`body` is the response text as-is when it's valid UTF-8. When it isn't
(binary responses — images, protobuf, etc.), `body` is base64 and
`body_encoding` is `"base64"` instead of `"utf8"`. Always check
`body_encoding` before treating `body` as text.

## Request body encoding

`body` is sent as UTF-8 text — binary request bodies (uploads, multipart
form-data) would be mangled. For those, set `body_base64` instead: the
base64-encoded bytes are decoded before sending. `body` and `body_base64`
are mutually exclusive (setting both is an error). Decoded size is capped
at 25 MiB. As with text bodies, set the right `Content-Type` via the
`headers` field — e.g. `Content-Type: multipart/form-data; boundary=...`
for a multipart upload built by the caller.

## Redirects

Disabled by default (`ACTION_OK` with the redirect's own 3xx status
returned as-is). Set `follow_redirects: true` to follow redirects, capped
by `max_redirects` (default `10`, clamped to `10` — the hard ceiling).
Every hop still resolves through `SsrfSafeResolver`, so a redirect to a
blocked host still fails the whole request.

## Per-caller concurrency cap

`http_request`s run concurrently — one slow request doesn't block the
caller's next one, and a caller can have several in flight at once (the
kernel matches responses by `action_id`, replies may come back out of
order). `NETWORK_PLUGIN_MAX_INFLIGHT_PER_CALLER` (default `8`, `0` =
unlimited) caps how many requests one calling plugin may have in flight at
once, so a single noisy plugin can't monopolize `network`'s outbound
connections while others starve. A request over the cap is rejected
immediately with an `ACTION_ERROR` naming the caller and the limit; slots
free up as in-flight requests complete. `network_stats` is never
cap-gated — it does not use the network.

## Compression

`gzip`, `brotli`, `deflate`, and `zstd` response bodies are decompressed
transparently (reqwest features enabled at build time). A response with
`Content-Encoding: gzip` arrives with the decompressed text in `body` —
callers never see compressed bytes or need to handle the encoding.

## Multipart bodies

`http_request` accepts a `multipart` param — an array of
`{name, value|file_base64, filename?, content_type?}` parts — instead of
`body`/`body_base64` (mutually exclusive with both). The plugin builds the
`multipart/form-data` wire body, generates the boundary, and overrides the
`Content-Type` header; a caller that previously hand-built multipart
uploads (e.g. the `stt` plugin's audio upload) can send parts directly.
Per-part and total decoded sizes are capped at 25 MiB, part names at
256 bytes, filenames at 1024 bytes; `"` in names/filenames is escaped and
CR/LF stripped so no part can break the framing.

## Per-request cache

`cache_ttl_ms` on `http_request` serves repeat requests from a bounded
in-memory, **per-caller** cache: a fresh hit returns the stored 2xx
response without any network egress, and the response carries
`"cache": "hit"` (`"miss"` when the request went to the origin). Only 2xx
responses are cached; `Cache-Control: no-store` responses are never
cached; the cache is keyed per caller+method+url+body, holds at most
128 entries / ~8 MiB (oldest evicted first), and is cleared on plugin
restart. Deliberate edge: a cache hit is served *before* the SSRF gates —
a URL that would now be blocked still returns the data the original caller
fetched earlier (no re-resolution, no egress; per-caller keying means only
the original caller can see its own cached data).

## Session cookies

`use_cookies: true` on `http_request` keeps a per-caller in-memory session
cookie jar: `Set-Cookie` headers on the response update it, and matching
cookies are attached to later requests to the same host. The caller's own
`Cookie` header wins over the jar. The jar is deliberately minimal — exact
host scoping only, no expiry/domain/path matching, in-memory (cleared on
restart), and cookie values are control-char-stripped so a hostile server
can't inject header framing. Enough for login-then-fetch session flows,
not a browser-grade cookie store.

## `network_stats`

`{totals: {requests, errors, avg_latency_ms}, per_caller: {caller: {...}}}`
— aggregated from every completed `http_request`. An `error` is a
transport/SSRF failure, no HTTP response (status 0), or an HTTP status
`>= 400`. Gated by `PERMISSION_NETWORK` like `http_request`; resets on
plugin restart.

## Allowlist mode (operator-configurable)

`NETWORK_PLUGIN_ALLOWED_HOSTS` (same comma-separated IP/hostname shape as
`NETWORK_PLUGIN_EXTRA_BLOCKED_HOSTS`) switches from default-block to
default-deny: when set, only listed hosts/IPs are reachable at all, and the
built-in RFC1918/loopback/etc. ranges stop being consulted for them (an
allowlist is an explicit statement that reaching that address, even a
private one, is intended). `NETWORK_PLUGIN_EXTRA_BLOCKED_HOSTS` still
applies on top as an override.

## TLS: custom CA bundle and client identity (mTLS)

- `NETWORK_PLUGIN_CA_BUNDLE_PATH` — path to one or more PEM-encoded CA
  certificates (concatenated), trusted in addition to the built-in root
  store. For internal APIs signed by a private CA.
- `NETWORK_PLUGIN_CLIENT_IDENTITY_PATH` — path to a single PEM file
  containing both the client certificate and its private key
  (concatenated), presented for mutual TLS.

Both are read once at startup; an invalid path or malformed PEM aborts
plugin startup rather than silently skipping TLS config.
