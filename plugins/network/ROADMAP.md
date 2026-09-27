# network plugin roadmap

Goal: make `network` the one blessed way any plugin does outbound network
I/O — nobody else opens sockets, everybody routes through here so SSRF
policy, egress control, and observability live in one place.

## Done

- **Response body as bytes, not lossy UTF-8** — `body` is UTF-8 text as-is
  when valid, else base64 with `body_encoding: "base64"`.
- **Header/URL size caps** — `MAX_URL_LEN` (8 KiB), `MAX_HEADER_COUNT`
  (100), `MAX_HEADERS_TOTAL_BYTES` (32 KiB), all rejected outright.
- **IPv6 SSRF test coverage** — loopback/unique-local/link-local/multicast
  and a public-IP allow case, mirroring the v4 tests.
- **Allowlist mode** — `NETWORK_PLUGIN_ALLOWED_HOSTS`, default-deny except
  listed hosts/IPs; `NETWORK_PLUGIN_EXTRA_BLOCKED_HOSTS` still overrides on
  top.
- **Redirect-follow, opt-in** — `follow_redirects: true` follows up to
  `MAX_REDIRECTS` (10, fixed) hops; every hop still resolves through
  `SsrfSafeResolver` via a second pre-built client
  (`NetworkPlugin::redirect_client`), sharing the same TLS/proxy config as
  the default client.
- **TLS client cert (mTLS) + custom CA bundle** —
  `NETWORK_PLUGIN_CA_BUNDLE_PATH` / `NETWORK_PLUGIN_CLIENT_IDENTITY_PATH`.
- **Structured JSON logging** — one JSON line per attempt to stdout.
- **Retry with backoff** — `max_retries`/`retry_backoff_ms`, retries only
  on 429/5xx/transport errors.
- **Deterministic failures are not retried** — SSRF-policy rejections
  (literal-IP and hostname hosts, pre-checked before the first attempt)
  and response bodies over the 10 MiB cap fail on the first attempt;
  retrying reproduces them exactly. A redirect hop rejected by the
  SSRF-gated policy is likewise not retried.
- **Opt-in proxy** — `NETWORK_PLUGIN_PROXY_URL`; ambient `HTTP_PROXY` env
  is now ignored (was a silent SSRF bypass before this was closed).
- **Concurrent request loop + per-caller concurrency cap** — `main.rs`
  hand-rolls the same concurrent serve-loop pattern `database` uses (one
  task owns the `VynkorClient`, spawned handlers reply via an mpsc
  channel), so multiple `http_request`s are genuinely in flight at once.
  `NETWORK_PLUGIN_MAX_INFLIGHT_PER_CALLER` (default 8, `0` = unlimited)
  rejects a caller's request once it has that many in flight, so one noisy
  plugin can't monopolize `network`'s outbound connections. Landed in
  0.2.0.
- **Configurable `max_redirects` per request** — new `max_redirects` param
  (default 10, clamped to `MAX_REDIRECTS`). One redirect-enabled client per
  distinct cap value (`0..=10`) instead of a single fixed client, so
  per-request limits don't forfeit connection pooling.
- **`network.request_completed` events** — every `http_request` now also
  publishes a `plugin.network.request_completed` event (kernel prepends the
  `plugin.<sender_id>.` namespace — subscribers subscribe to
  `plugin.network.request_completed` exactly, the event bus does no
  wildcard matching). Payload: `status` (0 when no HTTP response was
  obtained), `host`, `latency_ms`, `retry_count` (`attempts - 1`), `error`.
  Publish is best-effort via the `EventPublish` wire path
  (`PERMISSION_EVENT_PUBLISH`) and always trails the `ActionResponse`, so
  observability never delays or fails the caller's reply. Landed in 0.3.0.

## Near-term (buildable now, no kernel changes)

(none — the near-term items above shipped in 0.2.0.)

## v0.4 — feature batch (2026-08, agreed)

All buildable now. Status updated after implementation:

- **Compression** — enable reqwest `gzip`/`brotli`/`deflate`/`zstd` so
  `Content-Encoding` responses arrive decompressed (today they come back as
  garbage bytes/base64). Status: **done**.
- **`multipart` body** — `http_request` gains a `multipart` param building
  `multipart/form-data` from parts (`{name, value|file_base64, filename?,
  content_type?}`); mutually exclusive with `body`/`body_base64`; sets the
  boundary + `Content-Type`. Status: **done**.
- **`network_stats` action** — per-caller + totals counters (requests,
  errors, avg latency) tracked from each attempt; gated by
  `PERMISSION_NETWORK` like `http_request`. Status: **done**.
- **HTTP cache** — `cache_ttl_ms` param, bounded in-memory per-caller cache
  (2xx only, honors `Cache-Control: no-store`, oldest-eviction). Status:
  **done**.
- **Cookie jar** — `use_cookies` param, minimal per-caller in-memory
  session jar (host-scoped, Set-Cookie parse + Cookie attach; no
  expiry/domain/path matching beyond exact host; cleared on restart).
  Status: **done**.

## Requires kernel/protocol changes (see `KERNEL_PROTOCOL_TODO.md`, gitignored)

- **`http_request_stream` action** — avoid full-body buffering for large
  downloads/uploads. Blocked on a chunked-action wire primitive.
- **WebSocket support** — persistent bidirectional connections. Biggest
  lift; needs its own design pass in `vynkor-core` before any code here.
- **Kernel-enforced per-caller rate limits** — belt-and-suspenders on top of
  the near-term self-tracked concurrency cap above.

## "Standard network path" checklist

For `network` to be the thing every plugin reaches for instead of rolling
its own HTTP client, it needs, roughly in priority order:

1. Binary-safe responses (near-term item above) — text-only responses are
   a hard blocker for e.g. a plugin fetching an image.
2. Documented, stable JSON schema for `http_request`/response, versioned
   so other plugins don't need to track this plugin's internals directly.
3. Redirect support — a lot of real-world APIs redirect; disabling it
   entirely pushes callers back toward writing their own client.
4. Observability (events or at least parseable logs) so a caller plugin's
   failures are debuggable without SSHing in to read `network`'s stdout.
5. WebSocket, last — most plugins doing simple API calls only need HTTP;
   don't let this block the above.

## STAT-01 (audit 2026-09-27) — shipped in 0.1.1

The bare `status` action is now `network_status`. The kernel refuses to route
an action declared by more than one registered plugin (`ACTION_NOT_FOUND`),
and nine plugins declared `status`, so none of them was reachable.
