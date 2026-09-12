//! `network` plugin — outbound HTTP for other plugins/kernel, gated by
//! `PERMISSION_NETWORK`. See
//! docs/superpowers/specs/2026-07-05-network-plugin-design.md for the design.
//!
//! v1 is HTTP only. Needs real network egress: run with `sandbox: false`
//! in the kernel's `config.yaml` (see README.md).
//!
//! Concurrency: like `database`, this drives the SDK's concurrent message
//! loop ([`ConcurrentHandler`] + [`serve_concurrent`]) instead of the
//! sequential `Plugin::serve` (root ROADMAP.md, "hot-path plugins"). The
//! SDK loop spawns a handler task per inbound `ActionRequest` and funnels
//! completed replies back through an mpsc channel to the single task that
//! owns the client. Each `http_request` runs in its own task, so multiple
//! requests from one caller (and many callers) make progress in parallel,
//! and a per-caller in-flight cap
//! (`NETWORK_PLUGIN_MAX_INFLIGHT_PER_CALLER`) keeps one noisy caller from
//! monopolizing `network`'s outbound connections. The cap is enforced in
//! two places: [`ConcurrentHandler::accept`] (the loop's pre-spawn gate —
//! rejects without spawning a task) and the RAII slot guard in
//! [`ConcurrentHandler::on_action`] (the authoritative reservation, freed
//! even when the handler panics). Out-of-order replies are fine — the
//! kernel matches on `action_id`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use network_plugin::{handler, request};
use vynkor_sdk::concurrent::{response_envelope, serve_concurrent};
use vynkor_sdk::proto::{
    envelope, ActionRequest, Envelope, EventPublish, EventPublishStatus, PluginManifest,
};
use vynkor_sdk::{ConcurrentHandler, VynkorClient, VynkorError};

/// Operator-only opt-in proxy for all outbound requests. Deliberately not a
/// per-request param: a caller-controlled proxy would let any action bypass
/// `SsrfSafeResolver` entirely (the target host is resolved by the proxy,
/// not by us), so only an operator setting the plugin's own environment can
/// enable it.
const PROXY_URL_ENV: &str = "NETWORK_PLUGIN_PROXY_URL";

/// Operator-supplied extra CA cert(s) (PEM, one or more concatenated) to
/// trust in addition to the built-in root store — for internal APIs signed
/// by a private CA.
const CA_BUNDLE_PATH_ENV: &str = "NETWORK_PLUGIN_CA_BUNDLE_PATH";

/// Operator-supplied client identity (a single PEM file containing both the
/// client certificate and its private key, concatenated) for mTLS.
const CLIENT_IDENTITY_PATH_ENV: &str = "NETWORK_PLUGIN_CLIENT_IDENTITY_PATH";

/// Per-caller ceiling on how many `http_request`s may be in flight at once.
/// One noisy plugin can't monopolize `network`'s outbound connections while
/// others starve. `0` disables the cap (unlimited).
const MAX_INFLIGHT_PER_CALLER_ENV: &str = "NETWORK_PLUGIN_MAX_INFLIGHT_PER_CALLER";
const DEFAULT_MAX_INFLIGHT_PER_CALLER: usize = 8;

/// Everything operator-configurable that shapes a `reqwest::Client`, read
/// once at startup so per-cap redirect clients don't re-read env/files.
struct ClientConfig {
    proxy: Option<reqwest::Proxy>,
    ca_certs: Vec<reqwest::Certificate>,
    identity: Option<reqwest::Identity>,
}

impl ClientConfig {
    fn from_env() -> Self {
        let proxy = match std::env::var(PROXY_URL_ENV) {
            Ok(proxy_url) => {
                match reqwest::Proxy::all(&proxy_url) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        eprintln!("[network] WARNING: invalid {PROXY_URL_ENV}: {e}, ignoring proxy");
                        None
                    }
                }
            }
            Err(_) => None,
        };
        let mut ca_certs = Vec::new();
        if let Ok(ca_path) = std::env::var(CA_BUNDLE_PATH_ENV) {
            match std::fs::read(&ca_path) {
                Ok(pem) => {
                    match reqwest::Certificate::from_pem_bundle(&pem) {
                        Ok(certs) => ca_certs = certs,
                        Err(e) => {
                            eprintln!("[network] WARNING: invalid CA bundle at {ca_path}: {e}, ignoring");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[network] WARNING: failed to read {CA_BUNDLE_PATH_ENV} ({ca_path}): {e}, ignoring");
                }
            }
        }
        let identity = match std::env::var(CLIENT_IDENTITY_PATH_ENV) {
            Ok(identity_path) => {
                match std::fs::read(&identity_path) {
                    Ok(pem) => {
                        match reqwest::Identity::from_pem(&pem) {
                            Ok(id) => Some(id),
                            Err(e) => {
                                eprintln!("[network] WARNING: invalid client identity at {identity_path}: {e}, ignoring");
                                None
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[network] WARNING: failed to read {CLIENT_IDENTITY_PATH_ENV} ({identity_path}): {e}, ignoring");
                        None
                    }
                }
            }
            Err(_) => None,
        };
        Self {
            proxy,
            ca_certs,
            identity,
        }
    }
}

/// Per-caller session cookie jars: caller -> host -> (name, value).
type CookieJar = HashMap<String, HashMap<String, Vec<(String, String)>>>;

struct NetworkPlugin {
    /// Shared no-redirect client — the default path, so connection pooling
    /// is preserved for callers that don't follow redirects.
    client: reqwest::Client,
    /// One redirect-enabled client per distinct `max_redirects` value
    /// (`0..=request::MAX_REDIRECTS`, so `handle_http_request` can index it
    /// directly). Building per cap instead of per request keeps connection
    /// pooling for the common values while still honoring a caller's
    /// request-scoped limit.
    redirect_clients: Vec<reqwest::Client>,
    /// Same instances handed to `SsrfSafeResolver` for every client — kept
    /// here too so `handle_http_request` can run the literal-IP gate
    /// (`ssrf::check_literal_ip_host`) that the resolver can't cover, since
    /// it's only ever invoked for hostnames needing DNS resolution.
    extra_blocklist: network_plugin::ssrf::Blocklist,
    allowlist: network_plugin::ssrf::Allowlist,
    /// Per-caller in-flight cap for the `http_request` concurrency gate.
    inflight: Inflight,
    /// Per-caller request/error/latency counters for `network_stats`.
    stats: Mutex<HashMap<String, CallerStats>>,
    /// Per-caller bounded in-memory response cache (`cache_ttl_ms`).
    cache: Mutex<network_plugin::handler::CacheStore>,
    /// Per-caller session cookie jars (see [`CookieJar`]).
    jars: Mutex<CookieJar>,
}

/// Aggregated counters for one caller, read by the `network_stats` action.
#[derive(Debug, Default, Clone, Copy)]
struct CallerStats {
    requests: u64,
    errors: u64,
    latency_sum_ms: u64,
}

fn avg_latency_ms(s: &CallerStats) -> u64 {
    s.latency_sum_ms.checked_div(s.requests).unwrap_or(0)
}

impl NetworkPlugin {
    fn new() -> Self {
        let extra_blocklist = network_plugin::ssrf::Blocklist::from_env();
        let allowlist = network_plugin::ssrf::Allowlist::from_env();
        let config = ClientConfig::from_env();
        let max_inflight_per_caller = std::env::var(MAX_INFLIGHT_PER_CALLER_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_INFLIGHT_PER_CALLER);
        Self {
            client: Self::build_client(
                reqwest::redirect::Policy::none(),
                extra_blocklist.clone(),
                allowlist.clone(),
                &config,
            ),
            redirect_clients: (0..=request::MAX_REDIRECTS)
                .map(|cap| {
                    Self::build_client(
                        Self::redirect_policy(cap, extra_blocklist.clone(), allowlist.clone()),
                        extra_blocklist.clone(),
                        allowlist.clone(),
                        &config,
                    )
                })
                .collect(),
            extra_blocklist,
            allowlist,
            inflight: Inflight::new(max_inflight_per_caller),
            stats: Mutex::new(HashMap::new()),
            cache: Mutex::new(network_plugin::handler::CacheStore::new()),
            jars: Mutex::new(HashMap::new()),
        }
    }

    /// Redirect policy for `follow_redirects: true`, capped at
    /// `max_redirects` hops. Can't rely on `SsrfSafeResolver` alone here:
    /// it's a DNS resolver, and `reqwest` skips DNS resolution (and so the
    /// resolver) whenever a hop's target host is already a literal IP
    /// (`hyper-util`'s `HttpConnector::call_async`) — a redirect to
    /// `http://169.254.169.254/...` would otherwise sail through unguarded.
    /// Every hop's URL is checked here in addition to the resolver, which
    /// still runs for hostname hops.
    fn redirect_policy(
        max_redirects: usize,
        extra_blocklist: network_plugin::ssrf::Blocklist,
        allowlist: network_plugin::ssrf::Allowlist,
    ) -> reqwest::redirect::Policy {
        reqwest::redirect::Policy::custom(move |attempt| {
            // `previous()` includes the original URL, so `> max_redirects`
            // is what allows exactly `max_redirects` hops (reqwest's own
            // docs use `previous().len() > n` for "allow n redirects").
            if attempt.previous().len() > max_redirects {
                return attempt.stop();
            }
            let host = attempt.url().host_str().unwrap_or_default();
            match network_plugin::ssrf::check_literal_ip_host(host, &extra_blocklist, &allowlist)
            {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        })
    }

    /// Builds one `reqwest::Client` with every operator-configured option
    /// (SSRF resolver, proxy, CA bundle, client identity) applied — only
    /// `redirect` differs between the no-redirect client and the
    /// per-cap redirect clients, so all of them share the same
    /// TLS/proxy/SSRF posture instead of drifting apart.
    fn build_client(
        redirect_policy: reqwest::redirect::Policy,
        extra_blocklist: network_plugin::ssrf::Blocklist,
        allowlist: network_plugin::ssrf::Allowlist,
        config: &ClientConfig,
    ) -> reqwest::Client {
        // SSRF gating lives in `SsrfSafeResolver` (used for every connect,
        // including redirects) rather than a one-time pre-flight check —
        // see module docs on `SsrfSafeResolver`. That covers hostnames;
        // literal-IP hosts bypass it entirely (see `redirect_policy` and
        // `handle_http_request`), so this is deliberately not the only gate.
        let resolver = handler::SsrfSafeResolver {
            extra_blocklist,
            allowlist,
        };
        let mut builder = reqwest::Client::builder()
            .redirect(redirect_policy)
            .dns_resolver(Arc::new(resolver))
            // reqwest honors HTTP_PROXY/HTTPS_PROXY from the environment by
            // default; that would silently route requests around
            // SsrfSafeResolver. Turn it off — proxying is opt-in only via
            // `NETWORK_PLUGIN_PROXY_URL` below.
            .no_proxy();
        if let Some(proxy) = &config.proxy {
            builder = builder.proxy(proxy.clone());
        }
        for cert in &config.ca_certs {
            builder = builder.add_root_certificate(cert.clone());
        }
        if let Some(identity) = &config.identity {
            builder = builder.identity(identity.clone());
        }
        builder.build().expect("failed to build reqwest client")
    }

    fn client_for(&self, params: &request::HttpRequestParams) -> &reqwest::Client {
        if params.follow_redirects {
            &self.redirect_clients[params.max_redirects]
        } else {
            &self.client
        }
    }

    async fn handle_http_request(&self, caller_plugin_id: &str, params_json: &[u8]) -> HttpOutcome {
        let outcome = self.run_http_request(caller_plugin_id, params_json).await;
        self.record_stats(caller_plugin_id, &outcome);
        outcome
    }

    /// Count one completed request into the caller's `network_stats`
    /// counters. Errors = transport/SSRF failure (Err), no HTTP response
    /// (status 0), or an HTTP error status (>= 400).
    fn record_stats(&self, caller_plugin_id: &str, outcome: &HttpOutcome) {
        let mut stats = self.stats.lock().unwrap();
        let entry = stats.entry(caller_plugin_id.to_string()).or_default();
        entry.requests += 1;
        if outcome.response.is_err() || outcome.status == 0 || outcome.status >= 400 {
            entry.errors += 1;
        }
        entry.latency_sum_ms = entry.latency_sum_ms.saturating_add(outcome.latency_ms);
    }

    /// `{totals, per_caller}` counters payload for the `network_stats` action.
    fn network_stats_json(&self) -> Vec<u8> {
        let stats = self.stats.lock().unwrap();
        let mut per_caller = serde_json::Map::new();
        let mut totals = CallerStats::default();
        for (caller, s) in stats.iter() {
            totals.requests += s.requests;
            totals.errors += s.errors;
            totals.latency_sum_ms += s.latency_sum_ms;
            per_caller.insert(
                caller.clone(),
                serde_json::json!({
                    "requests": s.requests,
                    "errors": s.errors,
                    "avg_latency_ms": avg_latency_ms(s),
                }),
            );
        }
        serde_json::to_vec(&serde_json::json!({
            "totals": {
                "requests": totals.requests,
                "errors": totals.errors,
                "avg_latency_ms": avg_latency_ms(&totals),
            },
            "per_caller": per_caller,
        }))
        .unwrap_or_default()
    }

    async fn run_http_request(&self, caller_plugin_id: &str, params_json: &[u8]) -> HttpOutcome {
        let started = std::time::Instant::now();
        let params = match request::parse_request(params_json) {
            Ok(params) => params,
            Err(e) => {
                return HttpOutcome {
                    response: Err(e),
                    status: 0,
                    host: String::new(),
                    latency_ms: started.elapsed().as_millis() as u64,
                    retry_count: 0,
                }
            }
        };
        let host = url::Url::parse(&params.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();

        // Cache lookup happens BEFORE the SSRF gates on purpose: a cached
        // hit for a URL that would now be blocked still serves the data the
        // original caller fetched earlier (no re-resolution, no egress). The
        // cache is per-caller, so only the original caller can see its own
        // cached data.
        let cache_ttl = params.cache_ttl_ms;
        let cache_key_opt = (cache_ttl > 0).then(|| {
            handler::cache_key(
                caller_plugin_id,
                &params.method,
                &params.url,
                handler::request_body_hash(&params),
            )
        });
        if let Some(key) = &cache_key_opt {
            let now_ms = unix_millis();
            let store = self.cache.lock().unwrap();
            if let Some(entry) = store.get(key, now_ms) {
                let mut resp = handler::HttpResponseJson {
                    status: entry.status,
                    headers: entry.headers.clone(),
                    body: String::new(),
                    body_encoding: entry.body_encoding,
                    cache: Some("hit"),
                };
                resp.body = match entry.body_encoding {
                    "utf8" => String::from_utf8_lossy(&entry.body).into_owned(),
                    _ => {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.encode(&entry.body)
                    }
                };
                return HttpOutcome {
                    response: response_json(&resp),
                    status: entry.status,
                    host,
                    latency_ms: started.elapsed().as_millis() as u64,
                    retry_count: 0,
                };
            }
        }

        // `SsrfSafeResolver` never runs for a literal-IP host (see its gate
        // in `redirect_policy`'s doc comment) — this is the only check for
        // the initial URL in that case. Rejecting here also avoids wasting
        // `network`'s retry/backoff budget on a request that was never
        // going anywhere.
        if let Ok(url) = url::Url::parse(&params.url) {
            let host_str = url.host_str().unwrap_or_default();
            if let Err(e) = network_plugin::ssrf::check_literal_ip_host(
                host_str,
                &self.extra_blocklist,
                &self.allowlist,
            ) {
                return HttpOutcome {
                    response: Err(e),
                    status: 0,
                    host,
                    latency_ms: started.elapsed().as_millis() as u64,
                    retry_count: 0,
                };
            }
            // Hostname hosts: same fail-fast intent as the literal-IP gate.
            // The resolver is still authoritative at connect time (a name
            // can re-resolve between here and there — rebinding TOCTOU),
            // but a host that's blocked today fails here deterministically,
            // before the retry loop can burn attempts on it.
            if host_str.parse::<std::net::IpAddr>().is_err() && !host_str.is_empty() {
                if let Err(e) = handler::check_host_reachable(
                    host_str,
                    &self.extra_blocklist,
                    &self.allowlist,
                )
                .await
                {
                    return HttpOutcome {
                        response: Err(e),
                        status: 0,
                        host,
                        latency_ms: started.elapsed().as_millis() as u64,
                        retry_count: 0,
                    };
                }
            }
        }

        // Session cookies: a short-lived snapshot of the caller's jar for
        // this host, attached by the fetch path only when the caller sent
        // no `Cookie` header itself. `Set-Cookie` headers on the response
        // update the jar afterwards.
        let attach_cookies: Option<Vec<(String, String)>> = if params.use_cookies {
            self.jars
                .lock()
                .unwrap()
                .get(caller_plugin_id)
                .and_then(|jar| jar.get(&host))
                .cloned()
        } else {
            None
        };

        let outcome = handler::fetch_with_stats(
            self.client_for(&params),
            &params,
            attach_cookies.as_deref(),
            params.use_cookies,
        )
        .await;

        if params.use_cookies && !outcome.set_cookies.is_empty() && !host.is_empty() {
            let mut jars = self.jars.lock().unwrap();
            let list = jars
                .entry(caller_plugin_id.to_string())
                .or_default()
                .entry(host.clone())
                .or_default();
            for (name, value) in &outcome.set_cookies {
                if let Some(existing) = list.iter_mut().find(|(n, _)| n == name) {
                    existing.1 = value.clone();
                } else {
                    list.push((name.clone(), value.clone()));
                }
            }
        }

        let (response, status) = match outcome.response {
            Ok(mut resp) => {
                let status = resp.status;
                if let Some(key) = &cache_key_opt {
                    resp.cache = Some("miss");
                    if handler::is_cacheable(resp.status, &resp.headers) {
                        let body_bytes: Option<Vec<u8>> = match resp.body_encoding {
                            "utf8" => Some(resp.body.clone().into_bytes()),
                            _ => {
                                use base64::Engine;
                                base64::engine::general_purpose::STANDARD
                                    .decode(&resp.body)
                                    .ok()
                            }
                        };
                        if let Some(body_bytes) = body_bytes {
                            let now_ms = unix_millis();
                            let mut store = self.cache.lock().unwrap();
                            store.put(
                                key.clone(),
                                handler::CacheEntry {
                                    stored_at_ms: now_ms,
                                    expires_at_ms: now_ms.saturating_add(cache_ttl),
                                    status: resp.status,
                                    headers: resp.headers.clone(),
                                    body: body_bytes,
                                    body_encoding: resp.body_encoding,
                                },
                            );
                        }
                    }
                }
                (response_json(&resp), status)
            }
            Err(e) => (Err(e), 0),
        };
        HttpOutcome {
            response,
            status,
            host,
            latency_ms: started.elapsed().as_millis() as u64,
            retry_count: outcome.stats.attempts.saturating_sub(1),
        }
    }
}

/// Terminal outcome of one `http_request`: the response bytes (or error) the
/// caller gets in its `ActionResponse`, plus the data the best-effort
/// `network.request_completed` event carries. `spawn_handler` builds both
/// envelopes from this single value.
struct HttpOutcome {
    response: Result<Vec<u8>, String>,
    /// HTTP status of the final attempt; `0` when no HTTP response was
    /// obtained (SSRF-policy rejection, transport failure).
    status: u16,
    host: String,
    latency_ms: u64,
    /// `attempts - 1` — how many times the fetch was retried.
    retry_count: u32,
}

/// Best-effort `network.request_completed` event (kernel-namespaced as
/// `plugin.network.request_completed`). Fire-and-forget: it must never block
/// or alter the caller's response, so `spawn_handler` sends the
/// `ActionResponse` envelope first.
fn event_envelope(outcome: &HttpOutcome) -> Envelope {
    let payload_json = serde_json::to_vec(&serde_json::json!({
        "status": outcome.status,
        "host": outcome.host,
        "latency_ms": outcome.latency_ms,
        "retry_count": outcome.retry_count,
        "error": outcome.response.as_ref().err().cloned().unwrap_or_default(),
    }))
    .unwrap_or_else(|e| format!("{{\"error\":\"failed to encode event: {e}\"}}").into_bytes());
    Envelope {
        payload: Some(envelope::Payload::EventPublish(EventPublish {
            event_type: "request_completed".into(),
            payload_json,
        })),
        ..Default::default()
    }
}

/// Serialize the `http_request` response JSON. The `cache` field appears
/// only when caching was requested for the request (never as `null`).
fn response_json(resp: &handler::HttpResponseJson) -> Result<Vec<u8>, String> {
    let mut obj = serde_json::Map::new();
    obj.insert("status".into(), serde_json::json!(resp.status));
    obj.insert("headers".into(), serde_json::json!(resp.headers));
    obj.insert("body".into(), serde_json::json!(resp.body));
    obj.insert("body_encoding".into(), serde_json::json!(resp.body_encoding));
    if let Some(cache) = resp.cache {
        obj.insert("cache".into(), serde_json::json!(cache));
    }
    serde_json::to_vec(&serde_json::Value::Object(obj))
        .map_err(|e| format!("failed to encode response: {e}"))
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec![
            "PERMISSION_NETWORK".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: vec![
            "http_request".into(),
            "network_stats".into(),
        ],
        ..Default::default()
    }
}

/// Per-caller in-flight request tracker for the concurrency cap. The lock
/// is `std::sync::Mutex` on purpose: it is only ever held for the brief
/// increment/decrement, never across an `.await`, so a parking-lot/tokio
/// mutex's async semantics buy nothing here.
struct Inflight {
    limit: usize,
    active: Mutex<HashMap<String, usize>>,
}

/// RAII slot guard: frees the caller's in-flight slot on drop. Created by
/// [`Inflight::acquire`]; dropping it (including during unwinding from a
/// panicking handler) is what makes the cap panic-safe — the slot can
/// never leak.
struct InflightSlot<'a> {
    inflight: &'a Inflight,
    caller: String,
}

impl Drop for InflightSlot<'_> {
    fn drop(&mut self) {
        self.inflight.release(&self.caller);
    }
}

impl Inflight {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Pre-spawn gate, run in the loop task via
    /// [`ConcurrentHandler::accept`]: rejects a caller already at the cap
    /// *without* reserving a slot. The authoritative reservation happens in
    /// the spawned handler task ([`Inflight::acquire`]), which re-checks —
    /// a slot may have been taken between this check and the acquisition.
    fn check(&self, caller: &str) -> Result<(), String> {
        if self.limit == 0 {
            return Ok(());
        }
        let active = self.active.lock().unwrap();
        if let Some(in_flight) = active.get(caller) {
            if *in_flight >= self.limit {
                return Err(format!(
                    "caller {caller} already has {in_flight} requests in flight (limit {})",
                    self.limit
                ));
            }
        }
        Ok(())
    }

    /// Reserve a slot for `caller`, returning a guard that frees it on drop.
    /// Rejects (without reserving) when the caller is already at the cap —
    /// this is the authoritative gate inside the handler task.
    fn acquire(&self, caller: &str) -> Result<InflightSlot<'_>, String> {
        self.check(caller)?;
        if self.limit > 0 {
            let mut active = self.active.lock().unwrap();
            *active.entry(caller.to_string()).or_insert(0) += 1;
        }
        Ok(InflightSlot {
            inflight: self,
            caller: caller.to_string(),
        })
    }

    /// Free the slot a completed (or failed) request held for `caller`.
    fn release(&self, caller: &str) {
        if self.limit == 0 {
            return;
        }
        let mut active = self.active.lock().unwrap();
        if let Some(in_flight) = active.get_mut(caller) {
            *in_flight -= 1;
            if *in_flight == 0 {
                active.remove(caller);
            }
        }
    }
}

impl ConcurrentHandler for NetworkPlugin {
    fn id(&self) -> &str {
        "network"
    }

    fn version(&self) -> &str {
        "0.4.0"
    }

    fn manifest(&self) -> PluginManifest {
        manifest()
    }

    fn accept(&self, req: &ActionRequest) -> Result<(), String> {
        if req.action == "network_stats" {
            return Ok(()); // does not use the network — never cap-gated
        }
        self.inflight.check(&req.caller_plugin_id)
    }

    async fn on_action(&self, req: ActionRequest) -> Vec<Envelope> {
        if req.action == "network_stats" {
            return vec![response_envelope(req.action_id, Ok(self.network_stats_json()))];
        }
        if req.action != "http_request" {
            return vec![response_envelope(
                req.action_id,
                Err(format!("unknown action: {}", req.action)),
            )];
        }
        // Authoritative cap reservation. `accept` already rejected the
        // obviously-over-cap calls in the loop task, but the slot may
        // have been taken by another request since; re-check here. The
        // guard frees the slot when dropped — including when the
        // handler panics (the SDK loop turns that into an ACTION_ERROR).
        let _slot = match self.inflight.acquire(&req.caller_plugin_id) {
            Ok(slot) => slot,
            Err(error) => {
                return vec![response_envelope(req.action_id, Err(error))];
            }
        };
        let outcome = self
            .handle_http_request(&req.caller_plugin_id, &req.params_json)
            .await;
        let event = event_envelope(&outcome);
        let envelope = response_envelope(req.action_id, outcome.response);
        // Best-effort, sent only after the response so the reply never
        // waits on observability.
        vec![envelope, event]
    }

    async fn on_message(&self, env: Envelope) -> Result<Option<Envelope>, VynkorError> {
        match env.payload {
            Some(envelope::Payload::EventPublishAck(ack)) => {
                if ack.status != EventPublishStatus::EventPublishOk as i32 {
                    println!("[network] event publish failed: {:?}", ack.status);
                }
            }
            other => {
                println!("[network] unhandled message: {other:?}");
            }
        }
        Ok(None)
    }
}

/// Build the response envelope for a completed (or failed) action.
#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let plugin = Arc::new(NetworkPlugin::new());

    let client = VynkorClient::connect_from_env().await?;
    let token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    serve_concurrent(client, &token, plugin).await?;

    println!("[network] shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use vynkor_sdk::concurrent::run_concurrent_loop;
    use vynkor_sdk::proto::{ActionRequest, ActionStatus, PluginShutdown};
    use vynkor_sdk::VynkorClient;

    /// Plugin whose SSRF policy permits loopback, so tests can hit local
    /// mock servers (the real `NetworkPlugin::new()` reads operator env and
    /// would block 127.0.0.1).
    fn test_plugin() -> Arc<NetworkPlugin> {
        test_plugin_with_limit(0)
    }

    fn test_plugin_with_limit(max_inflight_per_caller: usize) -> Arc<NetworkPlugin> {
        let extra_blocklist = network_plugin::ssrf::Blocklist::default();
        let allowlist = network_plugin::ssrf::Allowlist::parse("127.0.0.1");
        let config = ClientConfig {
            proxy: None,
            ca_certs: Vec::new(),
            identity: None,
        };
        let client = NetworkPlugin::build_client(
            reqwest::redirect::Policy::none(),
            extra_blocklist.clone(),
            allowlist.clone(),
            &config,
        );
        let redirect_clients = (0..=request::MAX_REDIRECTS)
            .map(|cap| {
                NetworkPlugin::build_client(
                    NetworkPlugin::redirect_policy(cap, extra_blocklist.clone(), allowlist.clone()),
                    extra_blocklist.clone(),
                    allowlist.clone(),
                    &config,
                )
            })
            .collect();
        Arc::new(NetworkPlugin {
            client,
            redirect_clients,
            extra_blocklist,
            allowlist,
            inflight: Inflight::new(max_inflight_per_caller),
            stats: Mutex::new(HashMap::new()),
            cache: Mutex::new(network_plugin::handler::CacheStore::new()),
            jars: Mutex::new(HashMap::new()),
        })
    }

    async fn mock_server_responding(response: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                    let _ =
                        tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}/")
    }

    fn http_request(
        action_id: &str,
        caller: &str,
        params_json: Vec<u8>,
    ) -> Envelope {
        Envelope {
            payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                action_id: action_id.to_string(),
                action: "http_request".into(),
                params_json,
                timeout_ms: 0,
                streaming: false,
                caller_plugin_id: caller.to_string(),
            })),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn inflight_cap_tracks_and_releases_per_caller() {
        let inflight = Inflight::new(2);
        let slot_a1 = inflight.acquire("a").unwrap();
        let slot_a2 = inflight.acquire("a").unwrap();
        let err = match inflight.acquire("a") {
            Err(e) => e,
            Ok(_) => panic!("third concurrent slot for caller a should be rejected"),
        };
        assert!(err.contains("in flight"), "error was: {err}");
        // Different caller is unaffected — the cap is per caller, not global.
        assert!(inflight.acquire("b").is_ok());

        drop(slot_a1);
        let slot_a3 = inflight.acquire("a").unwrap();
        assert!(inflight.acquire("a").is_err());
        // Dropping a guard frees the slot; over-releasing (unknown or
        // already-freed callers) is a no-op, not a panic.
        drop(slot_a2);
        drop(slot_a3);
        assert!(inflight.acquire("a").is_ok());
        assert!(inflight.acquire("never_acquired").is_ok());
    }

    /// Regression test for the deadlock shape that motivated the concurrent
    /// loop (same pattern as `database`'s): fires a batch of
    /// `http_request`s back-to-back over a real `VynkorClient`
    /// (`UnixStream::pair` + `VynkorClient::from_stream` is the SDK's own
    /// test pattern) and then does *not* send anything until every response
    /// has been read back. The `tokio::time::timeout` wrapper turns "would
    /// have hung forever" into a clean failure.
    #[tokio::test]
    async fn concurrent_requests_get_responses_without_deadlocking() {
        let plugin = test_plugin();
        let url = mock_server_responding("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello").await;

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);

        let loop_task = tokio::spawn(run_concurrent_loop(client, plugin));

        const N: usize = 20;
        for i in 0..N {
            let params = serde_json::json!({"method": "GET", "url": url}).to_string();
            kernel
                .send(
                    "network",
                    http_request(&format!("action-{i}"), "caller_x", params.into_bytes()),
                )
                .await
                .unwrap();
        }

        let mut seen = std::collections::HashSet::new();
        let mut events = 0;
        while seen.len() < N || events < N {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for response — loop likely deadlocked")
                .unwrap();
            match env.payload {
                Some(envelope::Payload::ActionResponse(resp)) => {
                    assert_eq!(resp.status, ActionStatus::ActionOk as i32);
                    let v: serde_json::Value = serde_json::from_slice(&resp.data_json).unwrap();
                    assert_eq!(v["body"], "hello");
                    assert!(seen.insert(resp.action_id), "duplicate response");
                }
                Some(envelope::Payload::EventPublish(ev)) => {
                    assert_eq!(ev.event_type, "request_completed");
                    let v: serde_json::Value = serde_json::from_slice(&ev.payload_json).unwrap();
                    assert_eq!(v["status"], 200);
                    assert_eq!(v["retry_count"], 0, "no retries requested");
                    events += 1;
                }
                other => panic!("unexpected payload: {other:?}"),
            }
        }

        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                reason: "test done".into(),
                grace_seconds: 0,
            })),
            ..Default::default()
        };
        kernel.send("network", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("run_concurrent_loop did not exit after PluginShutdown")
            .unwrap()
            .unwrap();
    }

    /// The cap is enforced per caller and only while requests are actually
    /// in flight. A slow server holds accepted requests open long enough
    /// for the rejects to be observable; once the accepted ones finish,
    /// the slot frees up again.
    #[tokio::test]
    async fn per_caller_cap_rejects_over_limit_requests() {
        let plugin = test_plugin_with_limit(2);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = tokio::io::AsyncWriteExt::write_all(
                        &mut socket,
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
                    )
                    .await;
                });
            }
        });
        let url = format!("http://{addr}/");

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        let loop_task = tokio::spawn(run_concurrent_loop(client, plugin));

        for i in 0..5 {
            let params = serde_json::json!({"method": "GET", "url": url}).to_string();
            kernel
                .send(
                    "network",
                    http_request(&format!("x-{i}"), "caller_x", params.into_bytes()),
                )
                .await
                .unwrap();
        }
        // A different caller is never affected by caller_x's cap.
        let params = serde_json::json!({"method": "GET", "url": url}).to_string();
        kernel
            .send(
                "network",
                http_request("y-1", "caller_y", params.into_bytes()),
            )
            .await
            .unwrap();

        let mut ok = 0;
        let mut rejected = 0;
        let mut events = 0;
        while ok + rejected < 6 || events < 3 {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for response")
                .unwrap();
            match env.payload {
                Some(envelope::Payload::ActionResponse(resp)) => {
                    if resp.status == ActionStatus::ActionOk as i32 {
                        ok += 1;
                    } else {
                        assert!(
                            resp.error.contains("in flight"),
                            "unexpected rejection: {}",
                            resp.error
                        );
                        rejected += 1;
                    }
                }
                Some(envelope::Payload::EventPublish(ev)) => {
                    assert_eq!(ev.event_type, "request_completed");
                    events += 1;
                }                other => panic!("unexpected payload: {other:?}"),
            }
        }
        assert_eq!(ok, 3, "caller_x's 2 + caller_y's 1 should all succeed");
        assert_eq!(rejected, 3, "caller_x's remaining 3 should be rejected");
        assert_eq!(events, 3, "one event per accepted request, none for over-cap rejections");

        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                reason: "test done".into(),
                grace_seconds: 0,
            })),
            ..Default::default()
        };
        kernel.send("network", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("run_concurrent_loop did not exit after PluginShutdown")
            .unwrap()
            .unwrap();
    }

    /// `max_redirects` is honored per request: a chain of two redirects
    /// (A → B → C) followed with `max_redirects: 1` stops at B's 3xx;
    /// `max_redirects: 2` follows through to C.
    #[tokio::test]
    async fn redirect_max_caps_hops() {
        let plugin = test_plugin();
        let final_url = mock_server_responding("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await;
        let hop_b_url = {
            let location = format!("location: {final_url}\r\n");
            let response = format!("HTTP/1.1 302 Found\r\n{location}content-length: 0\r\n\r\n");
            mock_server_responding(Box::leak(response.into_boxed_str())).await
        };
        let hop_a_url = {
            let location = format!("location: {hop_b_url}\r\n");
            let response = format!("HTTP/1.1 302 Found\r\n{location}content-length: 0\r\n\r\n");
            mock_server_responding(Box::leak(response.into_boxed_str())).await
        };

        let capped = serde_json::json!({
            "method": "GET",
            "url": hop_a_url,
            "follow_redirects": true,
            "max_redirects": 1,
        })
        .to_string();
        let out = plugin.handle_http_request("caller_a", capped.as_bytes()).await.response.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["status"], 302, "one hop allowed: stops at B's 3xx");

        let full = serde_json::json!({
            "method": "GET",
            "url": hop_a_url,
            "follow_redirects": true,
            "max_redirects": 2,
        })
        .to_string();
        let out = plugin.handle_http_request("caller_a", full.as_bytes()).await.response.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["status"], 200, "two hops allowed: reaches C");
        assert_eq!(v["body"], "ok");
    }

    /// A successful request emits exactly one `request_completed` event with
    /// `retry_count = attempts - 1` (0 here: one attempt, no retries), the
    /// HTTP status, the target host, and a latency measurement.
    #[tokio::test]
    async fn request_completed_event_reports_retry_count_on_success() {
        let plugin = test_plugin();
        let url = mock_server_responding("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello").await;

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        let loop_task = tokio::spawn(run_concurrent_loop(client, plugin));

        let params = serde_json::json!({"method": "GET", "url": url}).to_string();
        kernel
            .send(
                "network",
                http_request("evt-ok", "caller_x", params.into_bytes()),
            )
            .await
            .unwrap();

        let mut response_seen = false;
        let mut event_seen = false;
        for _ in 0..2 {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for response")
                .unwrap();
            match env.payload {
                Some(envelope::Payload::ActionResponse(resp)) => {
                    assert_eq!(resp.status, ActionStatus::ActionOk as i32);
                    response_seen = true;
                }
                Some(envelope::Payload::EventPublish(ev)) => {
                    assert_eq!(ev.event_type, "request_completed");
                    let v: serde_json::Value = serde_json::from_slice(&ev.payload_json).unwrap();
                    assert_eq!(v["status"], 200);
                    assert_eq!(v["host"], "127.0.0.1");
                    assert_eq!(v["retry_count"], 0, "one attempt, no retries");
                    assert_eq!(v["error"], "");
                    assert!(v["latency_ms"].as_u64().is_some());
                    event_seen = true;
                }
                other => panic!("unexpected payload: {other:?}"),
            }
        }
        assert!(response_seen && event_seen, "expected a response and an event");

        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                reason: "test done".into(),
                grace_seconds: 0,
            })),
            ..Default::default()
        };
        kernel.send("network", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("run_concurrent_loop did not exit after PluginShutdown")
            .unwrap()
            .unwrap();
    }

    /// A request that ultimately fails still emits the event, with
    /// `retry_count = attempts - 1`: a server that drops the connection is a
    /// transport error (always retried), so `max_retries: 2` runs 3 attempts
    /// and the event reports `retry_count: 2`, `status: 0`, and an error
    /// string — the same `status`/`error` a subscriber needs to distinguish
    /// "never got an HTTP response" from a status-code failure.
    #[tokio::test]
    async fn request_completed_event_reports_retry_count_on_failure() {
        let plugin = test_plugin();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                    drop(socket);
                });
            }
        });
        let url = format!("http://{addr}/");

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        let loop_task = tokio::spawn(run_concurrent_loop(client, plugin));

        let params = serde_json::json!({
            "method": "GET",
            "url": url,
            "max_retries": 2,
            "retry_backoff_ms": 1,
        })
        .to_string();
        kernel
            .send(
                "network",
                http_request("evt-fail", "caller_x", params.into_bytes()),
            )
            .await
            .unwrap();

        let mut response_seen = false;
        let mut event_seen = false;
        for _ in 0..2 {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for response")
                .unwrap();
            match env.payload {
                Some(envelope::Payload::ActionResponse(resp)) => {
                    assert_eq!(resp.status, ActionStatus::ActionError as i32);
                    response_seen = true;
                }
                Some(envelope::Payload::EventPublish(ev)) => {
                    assert_eq!(ev.event_type, "request_completed");
                    let v: serde_json::Value = serde_json::from_slice(&ev.payload_json).unwrap();
                    assert_eq!(v["status"], 0, "no HTTP response was ever received");
                    assert_eq!(v["host"], "127.0.0.1");
                    assert_eq!(v["retry_count"], 2, "max_retries 2: 3 attempts, 2 retries");
                    assert!(!v["error"].as_str().unwrap().is_empty());
                    event_seen = true;
                }
                other => panic!("unexpected payload: {other:?}"),
            }
        }
        assert!(response_seen && event_seen, "expected a response and an event");

        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                reason: "test done".into(),
                grace_seconds: 0,
            })),
            ..Default::default()
        };
        kernel.send("network", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("run_concurrent_loop did not exit after PluginShutdown")
            .unwrap()
            .unwrap();
    }

    /// Serves `response` to every connection, counting them. Returns the URL
    /// and the shared connection counter.
    async fn counting_mock_server(response: &'static str) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                hits_clone.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}/"), hits)
    }

    #[tokio::test]
    async fn gzip_response_is_decompressed() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"decompressed hello").unwrap();
        let gzipped = encoder.finish().unwrap();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\n\r\n",
            gzipped.len()
        )
        .into_bytes();
        response.extend_from_slice(&gzipped);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, &response).await;
        });
        let params = serde_json::json!({"method": "GET", "url": format!("http://{addr}/")}).to_string();
        let plugin = test_plugin();
        let out = plugin.handle_http_request("caller_a", params.as_bytes()).await.response.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["status"], 200);
        assert_eq!(v["body"], "decompressed hello");
    }

    #[tokio::test]
    async fn cache_serves_repeat_request_without_second_server_hit() {
        use std::sync::atomic::Ordering;
        let plugin = test_plugin();
        let (url, hits) = counting_mock_server("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello").await;
        let params = serde_json::json!({
            "method": "GET", "url": url, "cache_ttl_ms": 60_000
        })
        .to_string();

        let first = plugin.handle_http_request("caller_a", params.as_bytes()).await.response.unwrap();
        let v1: serde_json::Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(v1["status"], 200);
        assert_eq!(v1["cache"], "miss");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let second = plugin.handle_http_request("caller_a", params.as_bytes()).await.response.unwrap();
        let v2: serde_json::Value = serde_json::from_slice(&second).unwrap();
        assert_eq!(v2["cache"], "hit");
        assert_eq!(v2["body"], "hello");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "cache hit must not touch the server again");
    }

    #[tokio::test]
    async fn cache_is_per_caller() {
        use std::sync::atomic::Ordering;
        let plugin = test_plugin();
        let (url, hits) = counting_mock_server("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello").await;
        let params = serde_json::json!({
            "method": "GET", "url": url, "cache_ttl_ms": 60_000
        })
        .to_string();

        plugin.handle_http_request("caller_a", params.as_bytes()).await;
        let out = plugin.handle_http_request("caller_b", params.as_bytes()).await.response.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["cache"], "miss", "caller B must not see caller A's cached data");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn non_2xx_response_is_not_cached() {
        use std::sync::atomic::Ordering;
        let plugin = test_plugin();
        let (url, hits) = counting_mock_server("HTTP/1.1 500 Server Error\r\ncontent-length: 0\r\n\r\n").await;
        let params = serde_json::json!({
            "method": "GET", "url": url, "cache_ttl_ms": 60_000
        })
        .to_string();

        plugin.handle_http_request("caller_a", params.as_bytes()).await;
        let out = plugin.handle_http_request("caller_a", params.as_bytes()).await.response.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["cache"], "miss", "5xx must not be cached — second request is a miss");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "server hit twice — nothing cached");
    }

    #[tokio::test]
    async fn stats_track_per_caller_requests_errors_and_latency() {
        let plugin = test_plugin();
        let (url, _) = counting_mock_server("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello").await;
        let ok_params = serde_json::json!({"method": "GET", "url": url}).to_string();
        plugin.handle_http_request("caller_a", ok_params.as_bytes()).await;
        plugin.handle_http_request("caller_a", ok_params.as_bytes()).await;

        let (err_url, _) = counting_mock_server("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n").await;
        let err_params = serde_json::json!({"method": "GET", "url": err_url}).to_string();
        plugin.handle_http_request("caller_a", err_params.as_bytes()).await;

        let stats = plugin.stats.lock().unwrap();
        let a = &stats["caller_a"];
        assert_eq!(a.requests, 3);
        assert_eq!(a.errors, 1, "404 counts as an error");
    }

    #[tokio::test]
    async fn network_stats_aggregates_totals_and_per_caller() {
        let plugin = test_plugin();
        let (url, _) = counting_mock_server("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello").await;
        let params = serde_json::json!({"method": "GET", "url": url}).to_string();
        plugin.handle_http_request("caller_a", params.as_bytes()).await;
        plugin.handle_http_request("caller_b", params.as_bytes()).await;

        let data = plugin.network_stats_json();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["totals"]["requests"], 2);
        assert_eq!(v["totals"]["errors"], 0);
        assert_eq!(v["per_caller"]["caller_a"]["requests"], 1);
        assert_eq!(v["per_caller"]["caller_b"]["requests"], 1);
        assert!(
            v["per_caller"]["caller_a"]["avg_latency_ms"].is_number(),
            "avg_latency_ms key present and numeric"
        );
    }

    #[tokio::test]
    async fn cookie_jar_roundtrips_set_cookie_to_followup_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen_clone = seen.clone();
        tokio::spawn(async move {
            for (i, response) in [
                "HTTP/1.1 200 OK\r\nset-cookie: session=abc123; Path=/\r\ncontent-length: 2\r\n\r\nok",
                "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
            ]
            .into_iter()
            .enumerate()
            {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 2048];
                let n = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                if i == 1 {
                    *seen_clone.lock().unwrap() = request;
                }
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            }
        });

        let plugin = test_plugin();
        let url = format!("http://{addr}/");
        let p1 = serde_json::json!({"method": "GET", "url": url, "use_cookies": true}).to_string();
        plugin.handle_http_request("caller_a", p1.as_bytes()).await;
        let p2 = serde_json::json!({"method": "GET", "url": url, "use_cookies": true}).to_string();
        plugin.handle_http_request("caller_a", p2.as_bytes()).await;

        let second_request = seen.lock().unwrap().clone();
        assert!(
            second_request.to_lowercase().contains("cookie: session=abc123"),
            "second request carries the jar cookie: {second_request}"
        );
    }
}
