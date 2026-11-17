//! The XDG GlobalShortcuts portal backend (`org.freedesktop.portal.Desktop`
//! → `org.freedesktop.portal.GlobalShortcuts`).
//!
//! This is the Wayland-native way to own global shortcuts: the compositor
//! grants combos through the desktop's own UI, and press-and-hold maps
//! exactly onto `Activated`/`Deactivated` signals — push-to-talk for free,
//! no evdev reading, no X11 grabs.
//!
//! One worker task owns one D-Bus connection and processes rebind commands
//! sequentially; every completed rebind replaces the whole session so the
//! portal's shortcut list always mirrors [`crate::bindings::BindingStore`].
//! Shortcut ids ARE binding ids — signals need no translation table beyond
//! the id itself.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SHORTCUTS_INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// A key event off the portal, ready to publish as a kernel event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalEvent {
    /// The binding id (= portal shortcut id).
    pub binding: String,
    /// `true` for `Activated`, `false` for `Deactivated`.
    pub pressed: bool,
}

/// Replace the whole bound set; the reply resolves after the portal
/// confirmed every shortcut (or with the failure reason).
pub struct Rebind {
    pub triggers: Vec<(String, String)>,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// Handle the serve loop keeps: commands in, key events out.
pub struct PortalHandle {
    pub cmd_tx: mpsc::Sender<Rebind>,
    pub events_rx: mpsc::Receiver<PortalEvent>,
}

/// Connect and verify the portal actually exists. Cheap enough to run at
/// plugin startup; a failure means "no desktop portal" and the caller
/// falls back to manual mode.
pub async fn connect() -> Result<Connection, String> {
    let conn = Connection::session()
        .await
        .map_err(|e| format!("session bus: {e}"))?;
    let proxy = Proxy::new(&conn, PORTAL_SERVICE, PORTAL_PATH, SHORTCUTS_INTERFACE)
        .await
        .map_err(|e| format!("portal proxy: {e}"))?;
    let version: u32 = proxy
        .get_property("version")
        .await
        .map_err(|e| format!("GlobalShortcuts portal unavailable: {e}"))?;
    if version < 1 {
        return Err(format!(
            "GlobalShortcuts portal version {version} is too old"
        ));
    }
    Ok(conn)
}

/// Spawn the worker task for an established connection.
pub fn spawn_worker(conn: Connection, app_session_token_prefix: impl Into<String>) -> PortalHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Rebind>(8);
    let (events_tx, events_rx) = mpsc::channel::<PortalEvent>(64);
    let prefix = app_session_token_prefix.into();
    tokio::spawn(async move {
        if let Err(e) = run_worker(conn, prefix, cmd_rx, events_tx).await {
            eprintln!("[hotkey] portal backend stopped: {e}");
        }
    });
    PortalHandle { cmd_tx, events_rx }
}

async fn run_worker(
    conn: Connection,
    token_prefix: String,
    mut cmds: mpsc::Receiver<Rebind>,
    events: mpsc::Sender<PortalEvent>,
) -> Result<(), String> {
    let proxy = Proxy::new(&conn, PORTAL_SERVICE, PORTAL_PATH, SHORTCUTS_INTERFACE)
        .await
        .map_err(|e| format!("portal proxy: {e}"))?;

    // Signal streams are created once and survive session recreation: they
    // match the interface, not a specific session path, and each message is
    // filtered by its session argument below.
    let mut activated = proxy
        .receive_signal("Activated")
        .await
        .map_err(|e| format!("subscribe Activated: {e}"))?;
    let mut deactivated = proxy
        .receive_signal("Deactivated")
        .await
        .map_err(|e| format!("subscribe Deactivated: {e}"))?;

    let mut session: Option<OwnedObjectPath> = None;
    let mut nonce: u64 = 0;

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(rebind) = cmd else { break };
                nonce += 1;
                let result = apply_bindings(
                    &conn, &proxy, &token_prefix, nonce, &rebind.triggers, &mut session,
                )
                .await;
                let _ = rebind.reply.send(result);
            }
            msg = activated.next() => {
                let Some(msg) = msg else { break };
                forward_key(&mut session, msg, true, &events).await;
            }
            msg = deactivated.next() => {
                let Some(msg) = msg else { break };
                forward_key(&mut session, msg, false, &events).await;
            }
        }
    }
    Ok(())
}

/// Decode one portal signal and forward it when it belongs to the current
/// session. Stale-session events (between rebinds) are dropped silently.
async fn forward_key(
    session: &mut Option<OwnedObjectPath>,
    msg: zbus::message::Message,
    pressed: bool,
    events: &mpsc::Sender<PortalEvent>,
) {
    let Ok((signal_session, shortcut_id, _ts, _opts)) =
        msg.body()
            .deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedValue>)>()
    else {
        return;
    };
    if session.as_ref() != Some(&signal_session) {
        return;
    }
    let _ = events
        .send(PortalEvent {
            binding: shortcut_id,
            pressed,
        })
        .await;
}

/// Close the old session (if any), create a fresh one, and bind the full
/// list. An empty list still runs the cycle — that's how "unbind
/// everything" releases the portal's grants.
async fn apply_bindings(
    conn: &Connection,
    proxy: &Proxy<'_>,
    token_prefix: &str,
    nonce: u64,
    triggers: &[(String, String)],
    session: &mut Option<OwnedObjectPath>,
) -> Result<(), String> {
    if let Some(old) = session.take() {
        close_session(conn, &old).await;
    }

    let session_handle = create_session(conn, proxy, token_prefix, nonce).await?;
    *session = Some(session_handle.clone());

    if triggers.is_empty() {
        return Ok(());
    }

    let (request_path, mut response_stream) =
        portal_request_stream(conn, token_prefix, nonce).await?;
    let shortcuts: Vec<(String, HashMap<&str, Value<'_>>)> = triggers
        .iter()
        .map(|(id, trigger)| {
            let mut props: HashMap<&str, Value<'_>> = HashMap::new();
            props.insert("description", Value::from(format!("hotkey {id}")));
            props.insert("preferred_trigger", Value::from(trigger.as_str()));
            (id.clone(), props)
        })
        .collect();
    let options: HashMap<&str, Value<'_>> = HashMap::new();

    let handle: OwnedObjectPath = proxy
        .call("BindShortcuts", &(session_handle, shortcuts, "", options))
        .await
        .map_err(|e| format!("BindShortcuts failed: {e}"))?;
    let (code, results) =
        await_portal_response(conn, handle, &request_path, &mut response_stream).await?;
    if code != 0 {
        return Err(format!(
            "shortcuts were denied by the desktop (response code {code})"
        ));
    }
    let bound: Vec<(String, HashMap<String, OwnedValue>)> = results
        .get("shortcuts")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| v.try_into().ok())
        .unwrap_or_default();
    for (id, _) in triggers {
        if !bound.iter().any(|(bound_id, _)| bound_id == id) {
            return Err(format!("shortcut '{id}' was not accepted by the portal"));
        }
    }
    Ok(())
}

async fn create_session(
    conn: &Connection,
    proxy: &Proxy<'_>,
    token_prefix: &str,
    nonce: u64,
) -> Result<OwnedObjectPath, String> {
    let (request_path, mut response_stream) =
        portal_request_stream(conn, token_prefix, nonce).await?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(last_component(&request_path)));
    options.insert(
        "session_handle_token",
        Value::from(format!("{token_prefix}_session_{nonce}")),
    );

    let handle: OwnedObjectPath = proxy
        .call("CreateSession", &(options))
        .await
        .map_err(|e| format!("CreateSession failed: {e}"))?;
    let (code, results) =
        await_portal_response(conn, handle, &request_path, &mut response_stream).await?;
    if code != 0 {
        return Err(format!(
            "shortcut session was denied by the desktop (response code {code})"
        ));
    }
    let handle: String = results
        .get("session_handle")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| "CreateSession response had no session_handle".to_string())?;
    OwnedObjectPath::try_from(handle).map_err(|e| format!("bad session_handle: {e}"))
}

/// Subscribe to the Request.Response signal BEFORE issuing the method call
/// (the portal may answer fast enough to race a late subscription).
async fn portal_request_stream<'a>(
    conn: &'a Connection,
    token_prefix: &str,
    nonce: u64,
) -> Result<(String, zbus::proxy::SignalStream<'a>), String> {
    let unique = conn
        .unique_name()
        .ok_or_else(|| "no unique bus name".to_string())?;
    let path = request_path(unique, &format!("{token_prefix}_req_{nonce}"));
    let request_proxy = Proxy::new(conn, PORTAL_SERVICE, path.as_str(), REQUEST_INTERFACE)
        .await
        .map_err(|e| format!("request proxy: {e}"))?;
    let stream = request_proxy
        .receive_signal("Response")
        .await
        .map_err(|e| format!("subscribe Response: {e}"))?;
    Ok((path, stream))
}

async fn await_portal_response(
    conn: &Connection,
    handle: OwnedObjectPath,
    expected_path: &str,
    stream: &mut zbus::proxy::SignalStream<'_>,
) -> Result<(u32, HashMap<String, OwnedValue>), String> {
    // Some portals route the reply to a server-chosen path; retarget the
    // stream when ours didn't predict it.
    if handle.as_str() != expected_path {
        *stream = Proxy::new(conn, PORTAL_SERVICE, handle.as_str(), REQUEST_INTERFACE)
            .await
            .map_err(|e| format!("request proxy: {e}"))?
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe Response: {e}"))?;
    }
    let msg = tokio::time::timeout(RESPONSE_TIMEOUT, stream.next())
        .await
        .map_err(|_| "timed out waiting for the portal response".to_string())?
        .ok_or_else(|| "portal response stream ended".to_string())?;
    msg.body()
        .deserialize::<(u32, HashMap<String, OwnedValue>)>()
        .map_err(|e| format!("malformed portal response: {e}"))
}

/// Politely release a session the desktop granted us (rebinds and worker
/// shutdown); portals also clean up on disconnect, this just isn't lazy.
async fn close_session(conn: &Connection, session: &OwnedObjectPath) {
    if let Ok(proxy) = Proxy::new(
        conn,
        PORTAL_SERVICE,
        session.as_str(),
        "org.freedesktop.portal.Session",
    )
    .await
    {
        let _: Result<(), _> = proxy.call("Close", &()).await;
    }
}

fn request_path(unique_name: &str, token: &str) -> String {
    format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        unique_name.trim_start_matches(':').replace('.', "_"),
        token
    )
}

fn last_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_paths_escape_the_unique_bus_name() {
        assert_eq!(
            request_path(":1.204", "hotkey_req_7"),
            "/org/freedesktop/portal/desktop/request/1_204/hotkey_req_7"
        );
        assert_eq!(last_component("/a/b/token9"), "token9");
        assert_eq!(last_component("bare"), "bare");
    }
}
