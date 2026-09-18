//! `hotkey` plugin binary — global key-combo triggers as kernel events.
//!
//! Single-reader serve loop (the calendar/sync-client pattern): the loop
//! owns the `VynkorClient`; a portal worker task owns the D-Bus connection
//! for the XDG GlobalShortcuts backend. Key events flow one way — worker →
//! loop → `publish_event` — and bind/unbind commands flow the other way
//! through a command channel with a completion reply, so `hotkey_bind`
//! fails honestly when the desktop denies a combo.
//!
//! Two backends behind identical actions/events:
//! - `portal` — XDG GlobalShortcuts (Wayland-native, press/release);
//! - `manual` — no OS integration; events come in via `hotkey_inject`
//!   (compositor exec binds, tests, custom wiring).
//!
//! The plugin publishes only (`PERMISSION_EVENT_PUBLISH`); it declares
//! `PERMISSION_SYSTEM` because global input interception is a system-level
//! capability even when the portal mediates it. A dedicated
//! `PERMISSION_HOTKEY` lands with the next wire enum bump (root ROADMAP).

use std::sync::Arc;

use hotkey_plugin::{bindings, portal, request};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, PluginManifest,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "hotkey";
const PLUGIN_VERSION: &str = "0.1.0";
const ACTIONS: [&str; 5] =
    ["hotkey_bind", "hotkey_unbind", "hotkey_list", "hotkey_inject", "hotkey_status"];

fn manifest() -> PluginManifest {
    PluginManifest {
        // System-level input surface (global combos); see module docs for
        // the dedicated-permission note.
        permissions: vec!["PERMISSION_SYSTEM".into(), "PERMISSION_EVENT_PUBLISH".into()],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
        action_specs: hotkey_plugin::manifest::action_specs(),
        ..Default::default()
    }
}

/// Shared between action handlers: the binding registry plus how rebinds
/// reach the portal (`None` in manual mode).
struct State {
    store: bindings::BindingStore,
    backend_name: &'static str,
    cmd_tx: Option<mpsc::Sender<portal::Rebind>>,
}

impl State {
    /// Replace the portal's whole shortcut set with the store's content.
    /// No-op in manual mode.
    async fn rebind_portal(&self) -> Result<(), String> {
        let Some(cmd_tx) = &self.cmd_tx else {
            return Ok(());
        };
        let triggers: Vec<(String, String)> = self
            .store
            .snapshot()
            .into_iter()
            .map(|b| (b.id, b.trigger))
            .collect();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        cmd_tx
            .send(portal::Rebind { triggers, reply: reply_tx })
            .await
            .map_err(|_| "portal backend stopped".to_string())?;
        match tokio::time::timeout(std::time::Duration::from_secs(20), reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("portal backend dropped the rebind".to_string()),
            Err(_) => Err("timed out waiting for the portal rebind".to_string()),
        }
    }
}

/// Resolve the operator's backend choice into a live handle. `manual`
/// short-circuits; anything else tries the portal first and degrades with
/// a log line rather than dying — an absent desktop portal is an
/// environment fact, not a plugin failure.
async fn resolve_backend(choice: &str) -> Result<Option<portal::PortalHandle>, String> {
    if choice.trim().eq_ignore_ascii_case("manual") {
        return Ok(None);
    }
    match portal::connect().await {
        Ok(conn) => Ok(Some(portal::spawn_worker(conn, "hotkey"))),
        Err(e) => {
            eprintln!(
                "[{PLUGIN_ID}] portal unavailable ({e}); falling back to manual \
                 (hotkey_inject drives events)"
            );
            Ok(None)
        }
    }
}

async fn serve(mut client: VynkorClient) -> Result<(), VynkorError> {
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client
        .register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token)
        .await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!(
            "registration rejected: {}",
            ack.reject_reason
        )));
    }
    println!("[{PLUGIN_ID}] registered with kernel");

    let backend_choice =
        std::env::var("HOTKEY_PLUGIN_BACKEND").unwrap_or_else(|_| "auto".to_string());
    let handle = resolve_backend(&backend_choice).await.map_err(VynkorError::Internal)?;
    let (backend_name, cmd_tx) = match &handle {
        Some(_) => {
            println!("[{PLUGIN_ID}] XDG GlobalShortcuts backend active");
            ("portal", Some(handle.as_ref().unwrap().cmd_tx.clone()))
        }
        None => {
            println!("[{PLUGIN_ID}] manual backend (events via hotkey_inject)");
            ("manual", None)
        }
    };
    // The key-event stream moves out of the handle so the select loop owns
    // its receiver without sharing mutable state through Arc.
    let mut portal_events = handle.map(|h| h.events_rx);

    println!("[{PLUGIN_ID}] backend: {backend_name}");

    let store = bindings::BindingStore::new();
    match std::env::var("HOTKEY_PLUGIN_BINDINGS") {
        Ok(spec) if !spec.trim().is_empty() => match bindings::parse_env_bindings(&spec) {
            Ok(list) => {
                for b in list {
                    println!("[{PLUGIN_ID}] bound {} → {}", b.id, b.trigger);
                    store.set(b);
                }
            }
            Err(e) => eprintln!(
                "[{PLUGIN_ID}] ignoring HOTKEY_PLUGIN_BINDINGS ({e}); fix the env and \
                 restart or use hotkey_bind"
            ),
        },
        _ => {}
    }

    let state = Arc::new(State { store, backend_name, cmd_tx });
    if state.cmd_tx.is_some() && !state.store.is_empty() {
        // Push the boot bindings into the portal; a failure doesn't kill
        // the plugin — binds stay correctable via hotkey_bind.
        if let Err(e) = state.rebind_portal().await {
            eprintln!("[{PLUGIN_ID}] initial portal bind failed: {e}");
        }
    }

    loop {
        tokio::select! {
            env = client.recv() => {
                let env = match env {
                    Ok(env) => env,
                    Err(_) => break,
                };
                match env.payload {
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let resp = handle_action(&mut client, &state, req).await;
                        let _ = client.send("kernel", resp).await;
                    }
                    Some(envelope::Payload::Ping(ping)) => {
                        let pong = Envelope {
                            payload: Some(envelope::Payload::Pong(
                                vynkor_sdk::proto::Pong {
                                    original_timestamp: ping.timestamp,
                                    server_timestamp: unix_millis(),
                                },
                            )),
                            ..Default::default()
                        };
                        let _ = client.send("kernel", pong).await;
                    }
                    Some(envelope::Payload::Event(event)) => {
                        // No subscriptions declared; ack defensively so the
                        // kernel never retries unexpected deliveries.
                        let _ = client.ack_event(&event.event_id).await;
                    }
                    Some(envelope::Payload::PluginShutdown(_)) => break,
                    _ => {}
                }
            }
            maybe_event = async {
                match portal_events.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(event) = maybe_event else { break };
                publish_key(&mut client, &event).await;
            }
        }
    }

    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

/// Publish one portal key event under the namespaced types consumers
/// subscribe to (`plugin.hotkey.hotkey_pressed` / `_released`). Best-effort
/// by contract: a failed publish logs instead of killing the loop.
async fn publish_key(client: &mut VynkorClient, event: &portal::PortalEvent) {
    let event_type = if event.pressed { "hotkey_pressed" } else { "hotkey_released" };
    let payload = json!({ "binding": event.binding });
    if let Err(e) = client.publish_event(event_type, payload.to_string().as_bytes(), 5000).await {
        eprintln!("[{PLUGIN_ID}] failed to publish {event_type}: {e}");
    }
}

async fn handle_action(
    client: &mut VynkorClient,
    state: &Arc<State>,
    req: ActionRequest,
) -> Envelope {
    let (data_json, error) = match dispatch(client, state, &req).await {
        Ok(data) => (data, String::new()),
        Err(error) => (Vec::new(), error),
    };
    Envelope {
        payload: Some(envelope::Payload::ActionResponse(ActionResponse {
            action_id: req.action_id,
            status: if error.is_empty() {
                ActionStatus::ActionOk as i32
            } else {
                ActionStatus::ActionError as i32
            },
            data_json,
            error,
        })),
        ..Default::default()
    }
}

async fn dispatch(
    client: &mut VynkorClient,
    state: &State,
    req: &ActionRequest,
) -> Result<Vec<u8>, String> {
    match request::parse(&req.action, &req.params_json)? {
        request::HotkeyRequest::Bind { id, trigger, description } => {
            state.store.set(bindings::Binding {
                id: id.clone(),
                trigger: trigger.clone(),
                description,
            });
            state.rebind_portal().await?;
            Ok(json!({
                "bound": true,
                "id": id,
                "trigger": trigger,
                "backend": state.backend_name,
            })
            .to_string()
            .into_bytes())
        }
        request::HotkeyRequest::Unbind { id } => {
            let removed = state.store.remove(&id);
            if removed {
                state.rebind_portal().await?;
            }
            Ok(json!({ "unbound": removed, "id": id }).to_string().into_bytes())
        }
        request::HotkeyRequest::List => {
            let list: Vec<Value> = state
                .store
                .snapshot()
                .into_iter()
                .map(|b| {
                    json!({
                        "id": b.id,
                        "trigger": b.trigger,
                        "description": b.description,
                    })
                })
                .collect();
            let payload = if req.action == "hotkey_status" {
                json!({ "backend": state.backend_name, "count": list.len() })
            } else {
                json!({
                    "backend": state.backend_name,
                    "count": list.len(),
                    "bindings": list,
                })
            };
            Ok(payload.to_string().into_bytes())
        }
        request::HotkeyRequest::Inject { binding, pressed } => {
            publish_key(
                client,
                &portal::PortalEvent { binding: binding.clone(), pressed },
            )
            .await;
            Ok(json!({
                "published": true,
                "binding": binding,
                "state": if pressed { "pressed" } else { "released" },
            })
            .to_string()
            .into_bytes())
        }
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let socket_path = std::env::var("VYN_SOCKET_PATH")
        .unwrap_or_else(|_| vynkor_wire::socket::default_socket_path());
    let secret = std::env::var("VYN_JWT_SECRET").ok().filter(|s| !s.is_empty());
    let client = match secret {
        Some(s) => VynkorClient::connect_with_secret(&socket_path, s.as_bytes()).await?,
        None => VynkorClient::connect(&socket_path).await?,
    };
    serve(client).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;
    use vynkor_sdk::proto::{EventPublishAck, EventPublishStatus, PluginRegisterAck};

    type Published = Arc<Mutex<Vec<(String, Value)>>>;

    /// Process env is global and cargo runs tests in parallel threads; every
    /// env-driven test holds this for its whole body so backend/bindings
    /// reads inside serve() see one consistent snapshot.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Clone)]
    struct Shim {
        tx: mpsc::Sender<Cmd>,
        published: Published,
    }

    enum Cmd {
        Call { action: String, params: Value, reply: tokio::sync::oneshot::Sender<Result<Value, String>> },
    }

    impl Shim {
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            let (reply, rx) = tokio::sync::oneshot::channel();
            self.tx
                .send(Cmd::Call { action: action.into(), params, reply })
                .await
                .expect("shim loop died");
            tokio::time::timeout(Duration::from_secs(5), rx)
                .await
                .expect("timed out waiting for plugin reply")
                .expect("shim dropped reply channel")
        }

        async fn published(&self) -> Vec<(String, Value)> {
            self.published.lock().await.clone()
        }
    }

    async fn wait_for_published(
        shim: &Shim,
        pred: impl Fn(&(String, Value)) -> bool + Copy,
    ) -> Option<(String, Value)> {
        for _ in 0..160 {
            let pubs = shim.published().await;
            if let Some(found) = pubs.iter().find(|p| pred(p)) {
                return Some(found.clone());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        None
    }

    /// Start the real `serve` against a fake kernel over a socket pair.
    /// Every action call is answered OK and every outbound EventPublish is
    /// recorded in arrival order — enough surface for the manual-mode
    /// behavior contract (the portal path needs a real desktop session bus).
    ///
    /// Env overrides land before serve() reads them; tests must pass unique
    /// values because process env is global and cargo runs tests in
    /// parallel threads.
    async fn start_manual_plugin(env_overrides: &[(&str, &str)]) -> Shim {
        let published: Published = Arc::new(Mutex::new(Vec::new()));
        let (tx, mut rx) = mpsc::channel::<Cmd>(32);
        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let kernel_client = VynkorClient::from_stream(kernel_side, None);
        let plugin_client = VynkorClient::from_stream(plugin_side, None);
        let pubs = Arc::clone(&published);

        for (k, v) in env_overrides {
            std::env::set_var(k, v);
        }

        tokio::spawn(async move {
            let mut seq: u64 = 0;
            let mut kernel = kernel_client;
            let mut registered = false;
            let mut pending: std::collections::HashMap<
                String,
                tokio::sync::oneshot::Sender<Result<Value, String>>,
            > = std::collections::HashMap::new();
            // Registration handshake FIRST (register_full treats the next
            // inbound frame as the ack); test commands queue until it lands.
            loop {
                let frame = tokio::select! {
                    biased;
                    cmd = rx.recv(), if registered => {
                        match cmd {
                            Some(Cmd::Call { action, params, reply }) => {
                                seq += 1;
                                let action_id = format!("t-{seq}");
                                pending.insert(action_id.clone(), reply);
                                let _ = kernel
                                    .send(
                                        "kernel",
                                        Envelope {
                                            payload: Some(envelope::Payload::ActionRequest(
                                                ActionRequest {
                                                    action_id,
                                                    action,
                                                    params_json: serde_json::to_vec(&params)
                                                        .unwrap(),
                                                    timeout_ms: 0,
                                                    streaming: false,
                                                    caller_plugin_id: "tester".into(),
                                                },
                                            )),
                                            ..Default::default()
                                        },
                                    )
                                    .await;
                            }
                            None => break,
                        }
                        continue;
                    }
                    env = kernel.recv() => env,
                };
                let env = match frame {
                    Ok(env) => env,
                    Err(_) => break,
                };
                match env.payload {
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        if let Some(reply) = pending.remove(&resp.action_id) {
                            let result = if resp.status == ActionStatus::ActionOk as i32 {
                                serde_json::from_slice::<Value>(&resp.data_json)
                                    .map_err(|e| format!("malformed payload: {e}"))
                            } else {
                                Err(resp.error)
                            };
                            let _ = reply.send(result);
                        }
                    }
                    Some(envelope::Payload::PluginRegister(reg)) => {
                        assert_eq!(reg.plugin_id, "hotkey");
                        let manifest = reg.manifest.unwrap_or_default();
                        let perms = manifest.permissions.clone();
                        assert!(perms.contains(&"PERMISSION_SYSTEM".to_string()));
                        assert!(perms.contains(&"PERMISSION_EVENT_PUBLISH".to_string()));
                        let actions = manifest.actions;
                        for action in ACTIONS {
                            assert!(actions.contains(&action.to_string()), "{action} missing");
                        }
                        ack_registration(&mut kernel).await;
                        registered = true;
                    }
                    Some(envelope::Payload::ActionRequest(req)) => {
                        seq += 1;
                        let _ = kernel
                            .send("kernel", action_ok(&req.action_id, json!({"echo": req.action})))
                            .await;
                    }
                    Some(envelope::Payload::EventPublish(ev)) => {
                        pubs.lock().await.push((
                            ev.event_type.clone(),
                            serde_json::from_slice(&ev.payload_json).unwrap_or(Value::Null),
                        ));
                        seq += 1;
                        let _ = kernel
                            .send(
                                "kernel",
                                Envelope {
                                    payload: Some(envelope::Payload::EventPublishAck(
                                        EventPublishAck {
                                            event_id: format!("ev-{seq}"),
                                            status: EventPublishStatus::EventPublishOk as i32,
                                            error: String::new(),
                                        },
                                    )),
                                    ..Default::default()
                                },
                            )
                            .await;
                    }
                    Some(envelope::Payload::Ping(ping)) => {
                        let _ = kernel
                            .send(
                                "kernel",
                                Envelope {
                                    payload: Some(envelope::Payload::Pong(
                                        vynkor_sdk::proto::Pong {
                                            original_timestamp: ping.timestamp,
                                            server_timestamp: 0,
                                        },
                                    )),
                                    ..Default::default()
                                },
                            )
                            .await;
                    }
                    Some(envelope::Payload::PluginShutdown(_)) | None => break,
                    _ => {}
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = serve(plugin_client).await {
                eprintln!("[plugin] serve error: {e}");
            }
        });

        Shim { tx, published }
    }

    async fn ack_registration(kernel: &mut VynkorClient) {
        let _ = kernel
            .send(
                "hotkey",
                Envelope {
                    payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck {
                        accepted: true,
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .await;
    }

    fn action_ok(action_id: &str, data: Value) -> Envelope {
        Envelope {
            payload: Some(envelope::Payload::ActionResponse(ActionResponse {
                action_id: action_id.into(),
                status: ActionStatus::ActionOk as i32,
                data_json: serde_json::to_vec(&data).unwrap(),
                error: String::new(),
            })),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn inject_publishes_pressed_and_released_with_the_daemon_payload_shape() {
        let _env = ENV_LOCK.lock().await;
        let shim = start_manual_plugin(&[
            ("HOTKEY_TEST_TAG", "inject"),
            ("HOTKEY_PLUGIN_BACKEND", "manual"),
            ("HOTKEY_PLUGIN_BINDINGS", ""),
        ])
        .await;

        shim.call("hotkey_inject", json!({"binding": "ptt", "state": "pressed"}))
            .await
            .unwrap();
        let press = wait_for_published(&shim, |(t, _)| t == "hotkey_pressed")
            .await
            .expect("pressed event missing");
        assert_eq!(press.1["binding"], "ptt", "daemon keys turns off this field");

        shim.call("hotkey_inject", json!({"binding": "ptt", "state": "released"}))
            .await
            .unwrap();
        let release = wait_for_published(&shim, |(t, _)| t == "hotkey_released")
            .await
            .expect("released event missing");
        assert_eq!(release.1["binding"], "ptt");

        let resp = shim
            .call("hotkey_inject", json!({"binding": "x", "state": "sideways"}))
            .await
            .unwrap_err();
        assert!(resp.contains("'pressed' or 'released'"), "{resp}");
    }

    #[tokio::test]
    async fn bind_list_unbind_roundtrip_reports_the_backend() {
        let _env = ENV_LOCK.lock().await;
        let shim = start_manual_plugin(&[
            ("HOTKEY_TEST_TAG", "roundtrip"),
            ("HOTKEY_PLUGIN_BACKEND", "manual"),
            ("HOTKEY_PLUGIN_BINDINGS", ""),
        ])
        .await;

        let empty = shim.call("hotkey_list", json!({})).await.unwrap();
        assert_eq!(empty["backend"], "manual");
        assert_eq!(empty["count"], 0);

        let bound = shim
            .call("hotkey_bind", json!({"id": "ptt", "trigger": "Ctrl+Shift+Space"}))
            .await
            .unwrap();
        assert_eq!(bound["bound"], true);
        assert_eq!(bound["trigger"], "CTRL+SHIFT+space");

        let listed = shim.call("hotkey_list", json!({})).await.unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["bindings"][0]["id"], "ptt");

        let status = shim.call("hotkey_status", json!({})).await.unwrap();
        assert_eq!(status["count"], 1);
        assert!(status.get("bindings").is_none(), "status stays lean");

        let unbound = shim.call("hotkey_unbind", json!({"id": "ptt"})).await.unwrap();
        assert_eq!(unbound["unbound"], true);

        // Unbind is idempotent like every other stop primitive in this
        // repo (cf. mic_stop): unknown id → {unbound: false}, no error.
        let ghost = shim.call("hotkey_unbind", json!({"id": "ghost"})).await.unwrap();
        assert_eq!(ghost["unbound"], false);
    }

    #[tokio::test]
    async fn boot_bindings_load_from_env_and_show_in_the_list() {
        let _env = ENV_LOCK.lock().await;
        let shim = start_manual_plugin(&[
            ("HOTKEY_TEST_TAG", "boot"),
            ("HOTKEY_PLUGIN_BACKEND", "manual"),
            ("HOTKEY_PLUGIN_BINDINGS", "ptt=Super+X;mute=Ctrl+F8"),
        ])
        .await;

        let listed = shim.call("hotkey_list", json!({})).await.unwrap();
        assert_eq!(listed["count"], 2);
        assert_eq!(listed["bindings"][0]["trigger"], "LOGO+x");
        assert_eq!(listed["bindings"][1]["trigger"], "CTRL+F8");
    }

    #[tokio::test]
    async fn invalid_boot_bindings_degrade_to_an_empty_store_not_a_dead_plugin() {
        let _env = ENV_LOCK.lock().await;
        let shim = start_manual_plugin(&[
            ("HOTKEY_TEST_TAG", "degrade"),
            ("HOTKEY_PLUGIN_BACKEND", "manual"),
            ("HOTKEY_PLUGIN_BINDINGS", "not-a-binding"),
        ])
        .await;

        let listed = shim.call("hotkey_list", json!({})).await.unwrap();
        assert_eq!(listed["count"], 0, "bad spec must not half-apply");
        let bound = shim
            .call("hotkey_bind", json!({"id": "ptt", "trigger": "Alt+Q"}))
            .await
            .unwrap();
        assert_eq!(bound["bound"], true);
    }
}
