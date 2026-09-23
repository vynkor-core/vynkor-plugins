//! Local MPRIS implementation for the `media` plugin.
//!
//! Talks to `org.mpris.MediaPlayer2.*` players on the session D-Bus via
//! `zbus` (session bus, object `/org/mpris/MediaPlayer2`,
//! interface `org.mpris.MediaPlayer2.Player`). All public functions are
//! testable: the D-Bus is accessed only through `MprisBackend` so tests use
//! `MockBackend`.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Serialize;
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::InterfaceName;
use zvariant::{ObjectPath, OwnedValue, Value};

use crate::resolve::{self, Candidate, Intent};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const NO_TRACK: &str = "/TrackList/NoTrack";

/// MPRIS players need not emit periodic Position updates; past this age an
/// extrapolated estimate is no longer trusted (BUG-2 window cap).
const EXTRAPOLATION_MAX_AGE_MS: u128 = 120_000;

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait MprisBackend: Send + Sync {
    async fn list_names(&self) -> Result<Vec<String>, String>;
    async fn call_player_method(
        &self,
        player: &str,
        method: &str,
        arg: Option<&str>,
    ) -> Result<(), String>;
    async fn seek_offset(&self, player: &str, offset_us: i64) -> Result<(), String>;
    async fn set_position(&self, player: &str, track_id: &str, position_us: i64) -> Result<(), String>;
    async fn get_position(&self, player: &str) -> Result<i64, String>;
    async fn get_volume(&self, player: &str) -> Result<f64, String>;
    async fn set_volume(&self, player: &str, volume: f64) -> Result<(), String>;
    async fn get_playback_status(&self, player: &str) -> Result<String, String>;
    async fn get_metadata(&self, player: &str) -> Result<HashMap<String, OwnedValue>, String>;
    async fn get_rate(&self, player: &str) -> Result<f64, String>;
    async fn get_shuffle(&self, player: &str) -> Result<bool, String>;
    async fn set_shuffle(&self, player: &str, enabled: bool) -> Result<(), String>;
    async fn get_loop_status(&self, player: &str) -> Result<String, String>;
    async fn set_loop_status(&self, player: &str, status: &str) -> Result<(), String>;
    async fn get_capability(&self, player: &str, prop: &str) -> Result<bool, String>;
    async fn call_root_method(&self, player: &str, method: &str) -> Result<(), String>;
    /// Whether each player's audio stream is actually heard; players with no
    /// matching stream are absent. Best-effort — an empty map just means
    /// resolution can't use audibility to break ties.
    async fn audible(&self, players: &[String]) -> HashMap<String, bool>;
}

// ---------------------------------------------------------------------------
// Real backend — zbus session bus
// ---------------------------------------------------------------------------

pub struct RealBackend;

#[async_trait]
impl MprisBackend for RealBackend {
    async fn list_names(&self) -> Result<Vec<String>, String> {
        let conn = crate::bus::session().await?;
        let proxy = DBusProxy::new(&conn)
            .await
            .map_err(|e| format!("ERR_MEDIA_BUS_UNAVAILABLE: {e}"))?;
        let names = proxy
            .list_names()
            .await
            .map_err(|e| format!("ERR_MEDIA_BUS_UNAVAILABLE: {e}"))?;
        Ok(names.into_iter().map(|n| n.to_string()).collect())
    }

    async fn call_player_method(
        &self,
        player: &str,
        method: &str,
        arg: Option<&str>,
    ) -> Result<(), String> {
        let conn = crate::bus::session().await?;
        if let Some(uri) = arg {
            conn.call_method(
                Some(player),
                MPRIS_PATH,
                Some(PLAYER_IFACE),
                method,
                &(uri,),
            )
            .await
            .map_err(|e| wrap_control_err(player, method, &e.to_string()))?;
        } else {
            conn.call_method(
                Some(player),
                MPRIS_PATH,
                Some(PLAYER_IFACE),
                method,
                &(),
            )
            .await
            .map_err(|e| wrap_control_err(player, method, &e.to_string()))?;
        }
        Ok(())
    }

    async fn seek_offset(&self, player: &str, offset_us: i64) -> Result<(), String> {
        let conn = crate::bus::session().await?;
        conn.call_method(
            Some(player),
            MPRIS_PATH,
            Some(PLAYER_IFACE),
            "Seek",
            &(offset_us,),
        )
        .await
        .map_err(|e| format!("ERR_MEDIA_SEEK_FAILED: player '{player}' Seek failed: {e}"))?;
        Ok(())
    }

    async fn set_position(&self, player: &str, track_id: &str, position_us: i64) -> Result<(), String> {
        let conn = crate::bus::session().await?;
        let path = ObjectPath::try_from(track_id)
            .map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: bad trackId '{track_id}': {e}"))?;
        conn.call_method(
            Some(player),
            MPRIS_PATH,
            Some(PLAYER_IFACE),
            "SetPosition",
            &(path, position_us),
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("UnknownMethod") || msg.contains("NotSupported") {
                format!("ERR_MEDIA_NOT_SUPPORTED: SetPosition not supported: {msg}")
            } else if msg.contains("Invalid") {
                format!("ERR_MEDIA_BAD_PARAMS: SetPosition invalid trackId/position: {msg}")
            } else {
                format!("ERR_MEDIA_SEEK_FAILED: player '{player}' SetPosition failed: {msg}")
            }
        })?;
        Ok(())
    }

    async fn get_position(&self, player: &str) -> Result<i64, String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Position failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Position failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Position failed: {e}"))?;
        let v: OwnedValue = props
            .get(iface.clone(), "Position")
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Position failed: {e}"))?;
        i64::try_from(v).map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: player '{player}' bad Position type: {e}"))
    }

    async fn get_volume(&self, player: &str) -> Result<f64, String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Volume failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Volume failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Volume failed: {e}"))?;
        let v: OwnedValue = props
            .get(iface.clone(), "Volume")
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Volume failed: {e}"))?;
        f64::try_from(v).map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: player '{player}' bad Volume type: {e}"))
    }

    async fn set_volume(&self, player: &str, volume: f64) -> Result<(), String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' set Volume failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' set Volume failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' set Volume failed: {e}"))?;
        let val = Value::new(volume);
        props
            .set(iface, "Volume", &val)
            .await
            .map_err(|e| wrap_control_err(player, "set Volume", &e.to_string()))?;
        Ok(())
    }

    async fn get_playback_status(&self, player: &str) -> Result<String, String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get PlaybackStatus failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get PlaybackStatus failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get PlaybackStatus failed: {e}"))?;
        let v: OwnedValue = props
            .get(iface.clone(), "PlaybackStatus")
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get PlaybackStatus failed: {e}"))?;
        String::try_from(v).map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: player '{player}' bad PlaybackStatus: {e}"))
    }

    async fn get_metadata(&self, player: &str) -> Result<HashMap<String, OwnedValue>, String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Metadata failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Metadata failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Metadata failed: {e}"))?;
        let v: OwnedValue = props
            .get(iface.clone(), "Metadata")
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Metadata failed: {e}"))?;
        HashMap::<String, OwnedValue>::try_from(v)
            .map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: player '{player}' bad Metadata type: {e}"))
    }

    async fn get_rate(&self, player: &str) -> Result<f64, String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Rate failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Rate failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Rate failed: {e}"))?;
        let v: OwnedValue = props
            .get(iface.clone(), "Rate")
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Rate failed: {e}"))?;
        f64::try_from(v).map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: player '{player}' bad Rate type: {e}"))
    }

    async fn get_shuffle(&self, player: &str) -> Result<bool, String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Shuffle failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Shuffle failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Shuffle failed: {e}"))?;
        let v: OwnedValue = props
            .get(iface.clone(), "Shuffle")
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get Shuffle failed: {e}"))?;
        bool::try_from(v).map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: player '{player}' bad Shuffle type: {e}"))
    }

    async fn set_shuffle(&self, player: &str, enabled: bool) -> Result<(), String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' set Shuffle failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' set Shuffle failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' set Shuffle failed: {e}"))?;
        let val = Value::new(enabled);
        props
            .set(iface, "Shuffle", &val)
            .await
            .map_err(|e| wrap_control_err(player, "set Shuffle", &e.to_string()))?;
        Ok(())
    }

    async fn get_loop_status(&self, player: &str) -> Result<String, String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get LoopStatus failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get LoopStatus failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get LoopStatus failed: {e}"))?;
        let v: OwnedValue = props
            .get(iface.clone(), "LoopStatus")
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' get LoopStatus failed: {e}"))?;
        String::try_from(v).map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: player '{player}' bad LoopStatus: {e}"))
    }

    async fn set_loop_status(&self, player: &str, status: &str) -> Result<(), String> {
        let normalized = match status.to_lowercase().as_str() {
            "none" => "None",
            "track" | "one" | "single" => "Track",
            "playlist" | "all" => "Playlist",
            _ => return Err(format!("ERR_MEDIA_BAD_PARAMS: invalid LoopStatus '{status}' (expected none/track/playlist)")),
        };
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' set LoopStatus failed: {e}"))?
            .path(MPRIS_PATH)
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' set LoopStatus failed: {e}"))?
            .build()
            .await
            .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' set LoopStatus failed: {e}"))?;
        let val = Value::new(normalized);
        props
            .set(iface, "LoopStatus", &val)
            .await
            .map_err(|e| wrap_control_err(player, "set LoopStatus", &e.to_string()))?;
        Ok(())
    }

    async fn call_root_method(&self, player: &str, method: &str) -> Result<(), String> {
        let conn = crate::bus::session().await?;
        conn.call_method(Some(player), MPRIS_PATH, Some("org.mpris.MediaPlayer2"), method, &()).await.map_err(|e| wrap_control_err(player, method, &e.to_string()))?;
        Ok(())
    }
    async fn get_capability(&self, player: &str, prop: &str) -> Result<bool, String> {
        let conn = crate::bus::session().await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();
        let props = PropertiesProxy::builder(&conn)
            .destination(player)
            .map_err(|e| wrap_control_err(player, &format!("get {prop}"), &e.to_string()))?
            .path(MPRIS_PATH)
            .map_err(|e| wrap_control_err(player, &format!("get {prop}"), &e.to_string()))?
            .build()
            .await
            .map_err(|e| wrap_control_err(player, &format!("get {prop}"), &e.to_string()))?;
        let v: OwnedValue = props
            .get(iface, prop)
            .await
            .map_err(|e| wrap_control_err(player, &format!("get {prop}"), &e.to_string()))?;
        bool::try_from(v).map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: player '{player}' bad {prop} type: {e}"))
    }

    async fn audible(&self, players: &[String]) -> HashMap<String, bool> {
        crate::audio::audible_map(players).await
    }
}

/// PID of the process owning an MPRIS bus name — the join key to its
/// PipeWire audio streams.
pub async fn player_pid(player: &str) -> Result<u32, String> {
    let conn = crate::bus::session().await?;
    let proxy = DBusProxy::new(&conn)
        .await
        .map_err(|e| format!("ERR_MEDIA_BUS_UNAVAILABLE: {e}"))?;
    let name = zbus::names::BusName::try_from(player)
        .map_err(|e| format!("ERR_MEDIA_BAD_PARAMS: bad player name '{player}': {e}"))?;
    proxy
        .get_connection_unix_process_id(name)
        .await
        .map_err(|e| wrap_control_err(player, "GetConnectionUnixProcessID", &e.to_string()))
}

// ---------------------------------------------------------------------------
// Signal watcher — PropertiesChanged / Seeked feed POS_CACHE (BUG-2 full fix)
// ---------------------------------------------------------------------------

static WATCHED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn watched() -> &'static Mutex<HashSet<String>> {
    WATCHED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Spawn the supervisor that keeps one signal-watcher task per MPRIS player.
/// Called from `on_init`; detached — it only ever writes to `POS_CACHE`, so
/// the single-reader rule on `VynkorClient` is untouched.
pub fn spawn_watch_task() {
    tokio::spawn(async move {
        let mut scan = tokio::time::interval(std::time::Duration::from_secs(5));
        scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            scan.tick().await;
            let Ok(players) = list_players().await else { continue };
            let mut new_watchers = Vec::new();
            {
                let mut set = watched().lock().unwrap();
                for p in players {
                    if set.insert(p.clone()) {
                        new_watchers.push(p);
                    }
                }
            }
            for p in new_watchers {
                tokio::spawn(watch_player(p));
            }
        }
    });
}

async fn watch_player(name: String) {
    // Any error ends this watcher; the next scan re-watches if the name is
    // listed again.
    let _ = run_watch(&name).await;
    watched().lock().unwrap().remove(&name);
}

async fn run_watch(name: &str) -> Result<(), String> {
    let conn = crate::bus::session().await?;

    let props = PropertiesProxy::builder(&conn)
        .destination(name.to_string())
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: watch '{name}': {e}"))?
        .path(MPRIS_PATH)
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: watch '{name}': {e}"))?
        .build()
        .await
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: watch '{name}': {e}"))?;
    let mut changed = props
        .receive_properties_changed()
        .await
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: watch '{name}': {e}"))?;

    let proxy = zbus::Proxy::new(&conn, name.to_string(), MPRIS_PATH, PLAYER_IFACE)
        .await
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: watch '{name}': {e}"))?;
    let mut seeked = proxy
        .receive_signal("Seeked")
        .await
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: watch '{name}': {e}"))?;

    loop {
        tokio::select! {
            item = changed.next() => {
                match item {
                    Some(sig) => handle_properties_changed(name, &sig),
                    None => return Ok(()),
                }
            }
            item = seeked.next() => {
                match item {
                    Some(msg) => {
                        if let Ok(pos_us) = msg.body().deserialize::<i64>() {
                            cache_note(name, Some(pos_us), None);
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

fn handle_properties_changed(player: &str, sig: &zbus::fdo::PropertiesChanged) {
    let Ok(args) = sig.args() else { return };
    if args.interface_name.as_str() != PLAYER_IFACE {
        return;
    }
    for (key, value) in args.changed_properties.iter() {
        match &**key {
            "Position" => {
                if let Ok(pos) = i64::try_from(value) {
                    cache_note(player, Some(pos), None);
                }
            }
            "Rate" => {
                if let Ok(rate) = f64::try_from(value) {
                    cache_note(player, None, Some(rate));
                }
            }
            "PlaybackStatus" => {
                if let Value::Str(s) = value {
                    note_status_change(player);
                    if s.as_str() != "Playing" {
                        cache_note(player, None, Some(0.0));
                    }
                }
            }
            _ => {}
        }
    }
}

fn cache_note(player: &str, pos: Option<i64>, rate: Option<f64>) {
    let mut cache = pos_cache().lock().unwrap();
    let entry = cache
        .entry(player.to_string())
        .or_insert((0, 1.0, Instant::now()));
    if let Some(p) = pos {
        entry.0 = p;
    }
    if let Some(r) = rate {
        entry.1 = r;
    }
    entry.2 = Instant::now();
}

// ---------------------------------------------------------------------------
// Helpers — error taxonomy + capability guards
// ---------------------------------------------------------------------------

/// Classify a raw D-Bus error message: `Some("VANISHED")` when the player
/// name is gone from the bus, `Some("NOT_SUPPORTED")` when the player is
/// alive but rejects the property/method. Anything else falls back per-call.
pub(crate) fn classify_dbus(msg: &str) -> Option<&'static str> {
    if msg.contains("ServiceUnknown")
        || msg.contains("NameHasNoOwner")
        || msg.contains("Disconnected")
    {
        Some("VANISHED")
    } else if msg.contains("No such property")
        || msg.contains("UnknownProperty")
        || msg.contains("UnknownMethod")
        || msg.contains("NotSupported")
    {
        Some("NOT_SUPPORTED")
    } else {
        None
    }
}

fn wrap_control_err(player: &str, op: &str, e: &str) -> String {
    match classify_dbus(e) {
        Some("NOT_SUPPORTED") => {
            format!("ERR_MEDIA_NOT_SUPPORTED: player '{player}' {op} failed: {e}")
        }
        _ => format!("ERR_MEDIA_PLAYER_VANISHED: player '{player}' {op} failed: {e}"),
    }
}

/// Capability probes are best-effort: only an explicit `false` blocks the
/// action; a missing/unreadable property must not break minimal players
/// (the actual failure then surfaces with its own taxonomy).
async fn require_capability<B: MprisBackend>(
    backend: &B,
    player: &str,
    prop: &str,
    op: &str,
) -> Result<(), String> {
    if matches!(backend.get_capability(player, prop).await, Ok(false)) {
        return Err(format!(
            "ERR_MEDIA_NOT_SUPPORTED: player '{player}' does not allow {op} ({prop}=false)"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers — player resolution
// ---------------------------------------------------------------------------

fn allowlist() -> Option<Vec<String>> {
    std::env::var("MEDIA_PLUGIN_PLAYERS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
}

fn is_allowed_with(list: &Option<Vec<String>>, name: &str) -> bool {
    match list {
        None => true,
        Some(v) => v.iter().any(|a| a == name),
    }
}

static LAST_CHANGE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn last_change() -> &'static Mutex<HashMap<String, Instant>> {
    LAST_CHANGE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that a player's PlaybackStatus just changed (from the watcher, or
/// from our own control call) — "resume" picks the most recent one.
fn note_status_change(player: &str) {
    last_change().lock().unwrap().insert(player.to_string(), Instant::now());
}

/// Resolve the target player(s) of a request. An explicit `player` accepts
/// the full bus name or a short one (`spotify`, `firefox`). Without it the
/// target comes from live state (see [`crate::resolve::choose`]) — there is
/// no configured default player.
async fn resolve_targets<B: MprisBackend>(
    backend: &B,
    requested: Option<&str>,
    intent: Intent,
) -> Result<Vec<String>, String> {
    let available = list_players_with(backend).await?;
    if let Some(r) = requested {
        return match resolve::match_requested(r, &available) {
            Ok(name) => Ok(vec![name]),
            Err(e) if allowlist().is_some() && e.starts_with("ERR_MEDIA_PLAYER_NOT_FOUND") => Err(format!(
                "ERR_MEDIA_PLAYER_NOT_ALLOWED: '{r}' not available or not in MEDIA_PLUGIN_PLAYERS (have: {available:?})"
            )),
            Err(e) => Err(e),
        };
    }

    let mut cands = Vec::with_capacity(available.len());
    for name in available {
        // A player that vanished between ListNames and here is just skipped.
        let Ok(status) = backend.get_playback_status(&name).await else { continue };
        let last = last_change().lock().unwrap().get(&name).copied();
        cands.push(Candidate { name, status, audible: None, last_change: last });
    }
    if resolve::needs_audibility(&cands) {
        let playing: Vec<String> =
            cands.iter().filter(|c| c.status == "Playing").map(|c| c.name.clone()).collect();
        let heard = backend.audible(&playing).await;
        for c in &mut cands {
            c.audible = heard.get(&c.name).copied();
        }
    }
    resolve::choose(&cands, intent)
}

/// Exactly one target player (explicit or the active one).
pub(crate) async fn resolve_one<B: MprisBackend>(backend: &B, requested: Option<&str>) -> Result<String, String> {
    let mut v = resolve_targets(backend, requested, Intent::One).await?;
    v.pop().ok_or_else(|| "ERR_MEDIA_NO_PLAYERS: no MPRIS players available".to_string())
}

/// The active player for a request without `player`, for callers outside
/// this module (stream actions target the active player's audio).
pub async fn active_player() -> Result<String, String> {
    resolve_one(&RealBackend, None).await
}

// ---------------------------------------------------------------------------
// Public free functions — used by main.rs
// ---------------------------------------------------------------------------

pub async fn list_players() -> Result<Vec<String>, String> {
    list_players_with(&RealBackend).await
}

async fn list_players_with<B: MprisBackend>(backend: &B) -> Result<Vec<String>, String> {
    let names = backend
        .list_names()
        .await
        .map_err(|e| format!("ERR_MEDIA_BUS_UNAVAILABLE: {e}"))?;
    let list = allowlist();
    let mut players: Vec<String> = names
        .into_iter()
        .filter(|n| n.starts_with(MPRIS_PREFIX) && is_allowed_with(&list, n))
        .collect();
    players.sort();
    Ok(players)
}

pub async fn play(player: Option<&str>, uri: Option<&str>) -> Result<serde_json::Value, String> {
    play_with(&RealBackend, player, uri).await
}

async fn play_with<B: MprisBackend>(
    backend: &B,
    player: Option<&str>,
    uri: Option<&str>,
) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    require_capability(backend, &target, "CanPlay", "play").await?;
    if let Some(u) = uri {
        backend.call_player_method(&target, "OpenUri", Some(u)).await?;
    }
    backend.call_player_method(&target, "Play", None).await?;
    note_status_change(&target);
    Ok(serde_json::json!({"ok": true, "player": target}))
}

pub async fn pause(player: Option<&str>) -> Result<serde_json::Value, String> {
    pause_with(&RealBackend, player).await
}
/// Without `player`, pauses every playing player ("pause the music").
async fn pause_with<B: MprisBackend>(backend: &B, player: Option<&str>) -> Result<serde_json::Value, String> {
    let targets = resolve_targets(backend, player, Intent::AllPlaying).await?;
    for target in &targets {
        require_capability(backend, target, "CanPause", "pause").await?;
        backend.call_player_method(target, "Pause", None).await?;
        note_status_change(target);
    }
    Ok(serde_json::json!({"ok": true, "players": targets}))
}

pub async fn play_pause(player: Option<&str>) -> Result<serde_json::Value, String> {
    play_pause_with(&RealBackend, player).await
}
async fn play_pause_with<B: MprisBackend>(backend: &B, player: Option<&str>) -> Result<serde_json::Value, String> {
    if player.is_none() {
        // Toggle without a target: something playing -> pause all of it,
        // silence -> resume the most recent player (handled below).
        let playing = resolve_targets(backend, None, Intent::AllPlaying).await?;
        if !playing.is_empty() {
            for target in &playing {
                require_capability(backend, target, "CanPause", "play_pause").await?;
                backend.call_player_method(target, "Pause", None).await?;
                note_status_change(target);
            }
            return Ok(serde_json::json!({"ok": true, "playing": false, "players": playing}));
        }
    }
    let target = resolve_one(backend, player).await?;
    require_capability(backend, &target, "CanPause", "play_pause").await?;
    backend.call_player_method(&target, "PlayPause", None).await?;
    // BUG-3 fix: PlaybackStatus updates async via PropertiesChanged, so retry
    // briefly instead of reading stale value immediately.
    let mut status = backend.get_playback_status(&target).await.unwrap_or_else(|_| "Paused".into());
    if status != "Playing" && status != "Paused" {
        status = "Paused".into();
    }
    // If we toggled, the status should flip within ~300ms. Poll with backoff.
    // We don't know prior state, but we retry up to 3 times if first read
    // looks stale (best-effort). Final value is reported regardless.
    for delay_ms in [50, 100, 150] {
        // Only retry if we suspect stale read: sleep and re-check.
        // Cheap for mock, correct for browsers.
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        if let Ok(new_status) = backend.get_playback_status(&target).await {
            if new_status != status {
                status = new_status;
                break;
            }
        }
    }
    note_status_change(&target);
    Ok(serde_json::json!({"ok": true, "playing": status == "Playing", "player": target}))
}

pub async fn next(player: Option<&str>) -> Result<serde_json::Value, String> {
    next_with(&RealBackend, player).await
}
async fn next_with<B: MprisBackend>(backend: &B, player: Option<&str>) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    require_capability(backend, &target, "CanGoNext", "next").await?;
    backend.call_player_method(&target, "Next", None).await?;
    Ok(serde_json::json!({"ok": true, "player": target}))
}

pub async fn prev(player: Option<&str>) -> Result<serde_json::Value, String> {
    prev_with(&RealBackend, player).await
}
async fn prev_with<B: MprisBackend>(backend: &B, player: Option<&str>) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    require_capability(backend, &target, "CanGoPrevious", "prev").await?;
    backend.call_player_method(&target, "Previous", None).await?;
    Ok(serde_json::json!({"ok": true, "player": target}))
}

pub async fn stop(player: Option<&str>) -> Result<serde_json::Value, String> {
    stop_with(&RealBackend, player).await
}
/// Without `player`, stops every playing player.
async fn stop_with<B: MprisBackend>(backend: &B, player: Option<&str>) -> Result<serde_json::Value, String> {
    let targets = resolve_targets(backend, player, Intent::AllPlaying).await?;
    for target in &targets {
        backend.call_player_method(target, "Stop", None).await?;
        note_status_change(target);
    }
    Ok(serde_json::json!({"ok": true, "players": targets}))
}

pub async fn seek(player: Option<&str>, position_ms: u64) -> Result<serde_json::Value, String> {
    seek_with(&RealBackend, player, position_ms).await
}
async fn seek_with<B: MprisBackend>(
    backend: &B,
    player: Option<&str>,
    position_ms: u64,
) -> Result<serde_json::Value, String> {
    let target_us = (position_ms as i64)
        .checked_mul(1000)
        .ok_or_else(|| "ERR_MEDIA_BAD_PARAMS: position_ms overflow".to_string())?;
    let target = resolve_one(backend, player).await?;
    let reached_us = seek_absolute_on(backend, &target, target_us).await?;
    Ok(serde_json::json!({"position_ms": reached_us / 1000, "player": target}))
}

pub async fn seek_relative(player: Option<&str>, offset_ms: i64) -> Result<serde_json::Value, String> {
    seek_relative_with(&RealBackend, player, offset_ms).await
}
async fn seek_relative_with<B: MprisBackend>(
    backend: &B,
    player: Option<&str>,
    offset_ms: i64,
) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    let current = backend
        .get_position(&target)
        .await
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: get Position failed: {e}"))?;
    let delta = offset_ms
        .checked_mul(1000)
        .ok_or_else(|| "ERR_MEDIA_BAD_PARAMS: offset_ms overflow".to_string())?;
    let target_us = current
        .checked_add(delta)
        .ok_or_else(|| "ERR_MEDIA_BAD_PARAMS: seek target overflow".to_string())?
        .max(0);
    let reached_us = seek_absolute_on(backend, &target, target_us).await?;
    Ok(serde_json::json!({"position_ms": reached_us / 1000, "player": target}))
}

/// Shared absolute-seek core: CanSeek guard, then `SetPosition` primary with
/// `Seek(offset)` fallback. Returns the requested target in micros (players
/// may land near it; exactness is not guaranteed by MPRIS).
async fn seek_absolute_on<B: MprisBackend>(
    backend: &B,
    target: &str,
    target_us: i64,
) -> Result<i64, String> {
    require_capability(backend, target, "CanSeek", "seek").await?;

    // Fetch trackId — required for SetPosition. If metadata unavailable,
    // fall back to Seek directly.
    let track_id_opt = match backend.get_metadata(target).await {
        Ok(raw) => parse_metadata(&raw).track_id,
        Err(_) => None,
    };

    if let Some(track_id) = track_id_opt {
        if track_id == NO_TRACK {
            return Err("ERR_MEDIA_NO_TRACK: no track loaded, cannot seek".to_string());
        }
        match backend.set_position(target, &track_id, target_us).await {
            Ok(()) => return Ok(target_us),
            Err(e) if e.contains("ERR_MEDIA_NOT_SUPPORTED") || e.contains("UnknownMethod") || e.contains("NotSupported") => {
                // Fall through to Seek fallback
            }
            Err(e) => return Err(e),
        }
    }

    let current = backend
        .get_position(target)
        .await
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: get Position failed: {e}"))?;
    let offset = target_us
        .checked_sub(current)
        .ok_or_else(|| "ERR_MEDIA_BAD_PARAMS: seek offset overflow".to_string())?;
    backend.seek_offset(target, offset).await?;
    Ok(target_us)
}

pub async fn set_volume(player: Option<&str>, level: f64) -> Result<serde_json::Value, String> {
    set_volume_with(&RealBackend, player, level).await
}
async fn set_volume_with<B: MprisBackend>(
    backend: &B,
    player: Option<&str>,
    level: f64,
) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    require_capability(backend, &target, "CanControl", "set volume").await?;
    let clamped = level.clamp(0.0, 1.0);
    backend.set_volume(&target, clamped).await?;
    Ok(serde_json::json!({"volume": clamped, "player": target}))
}

pub async fn status(player: Option<&str>) -> Result<serde_json::Value, String> {
    status_with(&RealBackend, player).await
}
async fn status_with<B: MprisBackend>(
    backend: &B,
    player: Option<&str>,
) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    let playback = backend
        .get_playback_status(&target)
        .await
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: {e}"))?;
    let volume = backend
        .get_volume(&target)
        .await
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: {e}"))?;
    let raw_position_us = backend
        .get_position(&target)
        .await
        .map_err(|e| format!("ERR_MEDIA_PLAYER_VANISHED: {e}"))?;
    let metadata_raw = backend.get_metadata(&target).await.unwrap_or_default();
    let fresh_meta = parse_metadata(&metadata_raw);
    let meta = merge_metadata_cached(&target, fresh_meta);

    let rate = backend
        .get_rate(&target)
        .await
        .unwrap_or(if playback == "Playing" { 1.0 } else { 0.0 });
    let shuffle = backend.get_shuffle(&target).await.unwrap_or(false);
    let loop_status = backend.get_loop_status(&target).await.unwrap_or_else(|_| "None".into());

    let position_us = extrapolate_position(&target, raw_position_us, rate, &playback);

    Ok(serde_json::json!({
        "player": target,
        "status": playback,
        "metadata": {
            "title": meta.title,
            "artists": meta.artists,
            "album": meta.album,
            "length_micros": meta.length_micros,
            "track_id": meta.track_id,
            "art_url": meta.art_url,
        },
        "volume": volume,
        "position_ms": position_us / 1000,
        "rate": rate,
        "shuffle": shuffle,
        "loop_status": loop_status
    }))
}

fn extrapolate_position(player: &str, raw_pos: i64, rate: f64, playback: &str) -> i64 {
    let now = Instant::now();
    let mut cache = pos_cache().lock().unwrap();
    let trusted = match cache.get(player) {
        Some(&(pos, rate, at)) if pos > 0 && now.duration_since(at).as_millis() <= EXTRAPOLATION_MAX_AGE_MS => {
            Some((pos, rate, at))
        }
        _ => None,
    };
    let result = if playback == "Playing" && rate != 0.0 && raw_pos == 0 {
        if let Some((prev_pos, prev_rate, prev_time)) = trusted {
            let elapsed_us = now.duration_since(prev_time).as_micros() as f64;
            let estimated = prev_pos as f64 + elapsed_us * prev_rate;
            if estimated > 0.0 {
                estimated as i64
            } else {
                raw_pos
            }
        } else {
            raw_pos
        }
    } else {
        raw_pos
    };
    let to_store = if playback == "Playing" && rate != 0.0 && result > 0 {
        result
    } else {
        raw_pos
    };
    cache.insert(player.to_string(), (to_store, rate, now));
    result
}


pub async fn raise(player: Option<&str>) -> Result<serde_json::Value, String> { raise_with(&RealBackend, player).await }
async fn raise_with<B: MprisBackend>(backend: &B, player: Option<&str>) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    backend.call_root_method(&target, "Raise").await?;
    Ok(serde_json::json!({"ok": true, "player": target}))
}
pub async fn quit(player: Option<&str>) -> Result<serde_json::Value, String> { quit_with(&RealBackend, player).await }
async fn quit_with<B: MprisBackend>(backend: &B, player: Option<&str>) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    backend.call_root_method(&target, "Quit").await?;
    Ok(serde_json::json!({"ok": true, "player": target}))
}

pub async fn set_shuffle(player: Option<&str>, enabled: bool) -> Result<serde_json::Value, String> {
    set_shuffle_with(&RealBackend, player, enabled).await
}
async fn set_shuffle_with<B: MprisBackend>(backend: &B, player: Option<&str>, enabled: bool) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    require_capability(backend, &target, "CanControl", "set shuffle").await?;
    backend.set_shuffle(&target, enabled).await?;
    Ok(serde_json::json!({"shuffle": enabled, "player": target}))
}

pub async fn set_loop(player: Option<&str>, mode: &str) -> Result<serde_json::Value, String> {
    set_loop_with(&RealBackend, player, mode).await
}
async fn set_loop_with<B: MprisBackend>(backend: &B, player: Option<&str>, mode: &str) -> Result<serde_json::Value, String> {
    let target = resolve_one(backend, player).await?;
    require_capability(backend, &target, "CanControl", "set loop").await?;
    backend.set_loop_status(&target, mode).await?;
    let normalized = backend.get_loop_status(&target).await.unwrap_or_else(|_| {
        match mode.to_lowercase().as_str() {
            "none" => "None".into(),
            "track" | "one" | "single" => "Track".into(),
            _ => "Playlist".into(),
        }
    });
    Ok(serde_json::json!({"loop_status": normalized, "player": target}))
}

// ---------------------------------------------------------------------------
// Metadata parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize)]
pub struct MediaMetadata {
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub length_micros: Option<i64>,
    pub track_id: Option<String>,
    pub art_url: Option<String>,
}

static META_CACHE: OnceLock<Mutex<HashMap<String, MediaMetadata>>> = OnceLock::new();
static POS_CACHE: OnceLock<Mutex<HashMap<String, (i64, f64, Instant)>>> = OnceLock::new();

fn meta_cache() -> &'static Mutex<HashMap<String, MediaMetadata>> {
    META_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pos_cache() -> &'static Mutex<HashMap<String, (i64, f64, Instant)>> {
    POS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn merge_metadata_cached(player: &str, fresh: MediaMetadata) -> MediaMetadata {
    let mut cache = meta_cache().lock().unwrap();
    let cached = cache.get(player).cloned().unwrap_or_default();
    let merged = MediaMetadata {
        title: fresh.title.or(cached.title),
        artists: if fresh.artists.is_empty() { cached.artists } else { fresh.artists },
        album: fresh.album.or(cached.album),
        length_micros: fresh.length_micros.or(cached.length_micros),
        track_id: fresh.track_id.or(cached.track_id),
        art_url: fresh.art_url.or(cached.art_url),
    };
    cache.insert(player.to_string(), merged.clone());
    merged
}

fn meta_string(metadata: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let v = metadata.get(key)?.try_clone().ok()?;
    match Value::try_from(v).ok()? {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(o) => Some(o.to_string()),
        _ => None,
    }
}

fn meta_string_array(metadata: &HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
    let Some(v) = metadata.get(key).and_then(|v| v.try_clone().ok()) else {
        return Vec::new();
    };
    match Value::try_from(v).ok() {
        Some(Value::Array(arr)) => arr.iter().filter_map(|e| e.downcast_ref::<String>().ok()).collect(),
        Some(Value::Str(s)) => vec![s.to_string()],
        _ => Vec::new(),
    }
}

fn parse_length_value(v: OwnedValue) -> Option<i64> {
    let val = Value::try_from(v).ok()?;
    let n = match val {
        Value::I64(n) => n,
        Value::U64(n) => n as i64,
        Value::I32(n) => n as i64,
        Value::U32(n) => n as i64,
        Value::I16(n) => n as i64,
        Value::U16(n) => n as i64,
        Value::U8(n) => n as i64,
        _ => return None,
    };
    (n >= 0).then_some(n)
}

pub fn parse_metadata(metadata: &HashMap<String, OwnedValue>) -> MediaMetadata {
    MediaMetadata {
        title: meta_string(metadata, "xesam:title"),
        artists: meta_string_array(metadata, "xesam:artist"),
        album: meta_string(metadata, "xesam:album"),
        length_micros: metadata
            .get("mpris:length")
            .and_then(|v| v.try_clone().ok())
            .and_then(parse_length_value),
        track_id: meta_string(metadata, "mpris:trackid"),
        art_url: meta_string(metadata, "mpris:artUrl"),
    }
}

// ---------------------------------------------------------------------------
// Tests — pure parsing + mock backend, no D-Bus needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zvariant::OwnedValue;

    fn s(v: &str) -> OwnedValue {
        OwnedValue::try_from(Value::new(v)).unwrap()
    }
    fn arr_str(vals: &[&str]) -> OwnedValue {
        let vec: Vec<String> = vals.iter().map(|s| s.to_string()).collect();
        OwnedValue::try_from(Value::new(vec)).unwrap()
    }
    fn i64v(n: i64) -> OwnedValue {
        OwnedValue::try_from(Value::new(n)).unwrap()
    }
    fn u64v(n: u64) -> OwnedValue {
        OwnedValue::try_from(Value::new(n)).unwrap()
    }
    fn u32v(n: u32) -> OwnedValue {
        OwnedValue::try_from(Value::new(n)).unwrap()
    }

    #[test]
    fn parse_empty() {
        let m: HashMap<String, OwnedValue> = HashMap::new();
        let p = parse_metadata(&m);
        assert!(p.title.is_none());
        assert!(p.artists.is_empty());
        assert!(p.length_micros.is_none());
    }

    #[test]
    fn parse_full() {
        let mut m = HashMap::new();
        m.insert("xesam:title".into(), s("Song"));
        m.insert("xesam:artist".into(), arr_str(&["Alice", "Bob"]));
        m.insert("xesam:album".into(), s("Album X"));
        m.insert("mpris:length".into(), i64v(210_000_000));
        m.insert("mpris:trackid".into(), s("/org/mpris/MediaPlayer2/Track/1"));
        m.insert("mpris:artUrl".into(), s("https://example.com/cover.jpg"));
        let p = parse_metadata(&m);
        assert_eq!(p.title.as_deref(), Some("Song"));
        assert_eq!(p.artists, vec!["Alice", "Bob"]);
        assert_eq!(p.album.as_deref(), Some("Album X"));
        assert_eq!(p.length_micros, Some(210_000_000));
        assert_eq!(p.track_id.as_deref(), Some("/org/mpris/MediaPlayer2/Track/1"));
        assert_eq!(p.art_url.as_deref(), Some("https://example.com/cover.jpg"));
    }

    #[test]
    fn parse_artist_single_string() {
        let mut m = HashMap::new();
        m.insert("xesam:artist".into(), s("Solo"));
        let p = parse_metadata(&m);
        assert_eq!(p.artists, vec!["Solo"]);
    }

    #[test]
    fn parse_artist_array() {
        let mut m = HashMap::new();
        m.insert("xesam:artist".into(), arr_str(&["A", "B", "C"]));
        let p = parse_metadata(&m);
        assert_eq!(p.artists, vec!["A", "B", "C"]);
    }

    #[test]
    fn parse_length_u64() {
        let mut m = HashMap::new();
        m.insert("mpris:length".into(), u64v(123_456_789));
        let p = parse_metadata(&m);
        assert_eq!(p.length_micros, Some(123_456_789));
    }

    #[test]
    fn parse_length_u32() {
        let mut m = HashMap::new();
        m.insert("mpris:length".into(), u32v(42_000_000));
        let p = parse_metadata(&m);
        assert_eq!(p.length_micros, Some(42_000_000));
    }

    #[test]
    fn parse_missing_length() {
        let mut m = HashMap::new();
        m.insert("xesam:title".into(), s("No length"));
        let p = parse_metadata(&m);
        assert!(p.length_micros.is_none());
    }

    struct MockBackend {
        names: Vec<String>,
        position: i64,
        status: String,
        track_id: Option<String>,
        set_position_ok: bool,
        rate: f64,
        shuffle: bool,
        loop_status: String,
        caps: HashMap<String, bool>,
        /// Per-player PlaybackStatus overrides (else `status`).
        statuses: HashMap<String, String>,
        audible: HashMap<String, bool>,
        calls: Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl MprisBackend for MockBackend {
        async fn list_names(&self) -> Result<Vec<String>, String> {
            Ok(self.names.clone())
        }
        async fn call_player_method(&self, player: &str, method: &str, _arg: Option<&str>) -> Result<(), String> {
            self.calls.lock().unwrap().push((player.to_string(), method.to_string()));
            Ok(())
        }
        async fn seek_offset(&self, _player: &str, _offset: i64) -> Result<(), String> {
            Ok(())
        }
        async fn set_position(&self, _player: &str, _track_id: &str, _pos: i64) -> Result<(), String> {
            if self.set_position_ok { Ok(()) } else { Err("ERR_MEDIA_NOT_SUPPORTED: mock".into()) }
        }
        async fn get_position(&self, _player: &str) -> Result<i64, String> { Ok(self.position) }
        async fn get_volume(&self, _player: &str) -> Result<f64, String> { Ok(0.5) }
        async fn set_volume(&self, _player: &str, _v: f64) -> Result<(), String> { Ok(()) }
        async fn get_playback_status(&self, player: &str) -> Result<String, String> {
            Ok(self.statuses.get(player).cloned().unwrap_or_else(|| self.status.clone()))
        }
        async fn get_metadata(&self, _player: &str) -> Result<HashMap<String, OwnedValue>, String> {
            let mut m = HashMap::new();
            if let Some(tid) = &self.track_id {
                m.insert("mpris:trackid".into(), s(tid));
            }
            Ok(m)
        }
        async fn get_rate(&self, _player: &str) -> Result<f64, String> { Ok(self.rate) }
        async fn get_shuffle(&self, _player: &str) -> Result<bool, String> { Ok(self.shuffle) }
        async fn set_shuffle(&self, _player: &str, _v: bool) -> Result<(), String> { Ok(()) }
        async fn get_loop_status(&self, _player: &str) -> Result<String, String> { Ok(self.loop_status.clone()) }
        async fn set_loop_status(&self, _player: &str, _s: &str) -> Result<(), String> { Ok(()) }
        async fn call_root_method(&self, _player: &str, _method: &str) -> Result<(), String> { Ok(()) }
        async fn get_capability(&self, _player: &str, prop: &str) -> Result<bool, String> {
            Ok(*self.caps.get(prop).unwrap_or(&true))
        }
        async fn audible(&self, _players: &[String]) -> HashMap<String, bool> {
            self.audible.clone()
        }
    }

    fn mock(names: Vec<&str>, position: i64, status: &str, track_id: Option<&str>) -> MockBackend {
        MockBackend {
            names: names.into_iter().map(|s| s.to_string()).collect(),
            position,
            status: status.into(),
            track_id: track_id.map(|s| s.to_string()),
            set_position_ok: true,
            rate: 1.0,
            shuffle: false,
            loop_status: "None".into(),
            caps: HashMap::new(),
            statuses: HashMap::new(),
            audible: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    const SPOTIFY: &str = "org.mpris.MediaPlayer2.spotify";
    const MPD: &str = "org.mpris.MediaPlayer2.mpd";
    const FIREFOX: &str = "org.mpris.MediaPlayer2.firefox.instance_1_424";

    /// mpd paused (sorts first), spotify + firefox as given.
    fn three(spotify: &str, firefox: &str) -> MockBackend {
        let mut b = mock(vec![FIREFOX, MPD, SPOTIFY], 0, "Paused", None);
        b.statuses.insert(SPOTIFY.into(), spotify.into());
        b.statuses.insert(FIREFOX.into(), firefox.into());
        b
    }

    fn called(b: &MockBackend) -> Vec<(String, String)> {
        b.calls.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn active_player_is_the_playing_one_not_the_first() {
        // Regression: the old default resolved to `mpd` (alphabetically first)
        // while Spotify was the one playing.
        let b = three("Playing", "Paused");
        let v = next_with(&b, None).await.unwrap();
        assert_eq!(v["player"], SPOTIFY);
        assert_eq!(called(&b), vec![(SPOTIFY.to_string(), "Next".to_string())]);
    }

    #[tokio::test]
    async fn two_playing_is_ambiguous_for_single_target_actions() {
        let b = three("Playing", "Playing");
        let err = next_with(&b, None).await.unwrap_err();
        assert!(err.starts_with("ERR_MEDIA_AMBIGUOUS"), "{err}");
        assert!(called(&b).is_empty());
    }

    #[tokio::test]
    async fn audible_stream_breaks_the_tie() {
        let mut b = three("Playing", "Playing");
        b.audible.insert(SPOTIFY.into(), false);
        b.audible.insert(FIREFOX.into(), true);
        let v = seek_relative_with(&b, None, 1000).await.unwrap();
        assert_eq!(v["player"], FIREFOX);
    }

    #[tokio::test]
    async fn pause_without_player_pauses_everything_playing() {
        let b = three("Playing", "Playing");
        let v = pause_with(&b, None).await.unwrap();
        assert_eq!(v["players"], serde_json::json!([FIREFOX, SPOTIFY]));
        assert_eq!(
            called(&b),
            vec![(FIREFOX.to_string(), "Pause".to_string()), (SPOTIFY.to_string(), "Pause".to_string())]
        );
    }

    #[tokio::test]
    async fn pause_when_nothing_plays_is_a_noop() {
        let b = three("Paused", "Stopped");
        let v = pause_with(&b, None).await.unwrap();
        assert_eq!(v["players"], serde_json::json!([]));
        assert!(called(&b).is_empty());
    }

    #[tokio::test]
    async fn play_pause_without_player_pauses_all_playing() {
        let b = three("Playing", "Playing");
        let v = play_pause_with(&b, None).await.unwrap();
        assert_eq!(v["playing"], false);
        assert!(called(&b).iter().all(|(_, m)| m == "Pause"));
        assert_eq!(called(&b).len(), 2);
    }

    #[tokio::test]
    async fn play_resumes_most_recently_changed_player() {
        let b = three("Paused", "Stopped");
        note_status_change(SPOTIFY);
        let v = play_with(&b, None, None).await.unwrap();
        assert_eq!(v["player"], SPOTIFY);
    }

    #[tokio::test]
    async fn explicit_short_player_name_resolves() {
        let b = three("Paused", "Playing");
        let v = next_with(&b, Some("spotify")).await.unwrap();
        assert_eq!(v["player"], SPOTIFY);
        let v = next_with(&b, Some("firefox")).await.unwrap();
        assert_eq!(v["player"], FIREFOX);
    }

    #[tokio::test]
    async fn explicit_unknown_player_is_not_found() {
        let b = three("Paused", "Playing");
        let err = next_with(&b, Some("vlc")).await.unwrap_err();
        assert!(err.starts_with("ERR_MEDIA_PLAYER_NOT_FOUND"), "{err}");
    }

    #[tokio::test]
    async fn list_players_filters_prefix() {
        let b = mock(vec!["org.mpris.MediaPlayer2.spotify", "org.freedesktop.DBus", ":1.42"], 0, "Playing", None);
        let players = list_players_with(&b).await.unwrap();
        assert_eq!(players, vec!["org.mpris.MediaPlayer2.spotify"]);
    }

    #[tokio::test]
    async fn seek_no_track_rejects() {
        let b = mock(vec!["org.mpris.MediaPlayer2.mpd"], 0, "Stopped", Some("/TrackList/NoTrack"));
        let err = seek_with(&b, None, 5000).await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NO_TRACK"));
    }

    #[tokio::test]
    async fn seek_uses_set_position_primary() {
        let b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Playing", Some("/org/mpris/MediaPlayer2/Track/1"));
        let res = seek_with(&b, None, 5000).await.unwrap();
        assert_eq!(res["position_ms"], 5000);
    }

    #[tokio::test]
    async fn seek_fallback_to_seek_when_set_position_unsupported() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 1000, "Playing", Some("/org/mpris/MediaPlayer2/Track/1"));
        b.set_position_ok = false;
        let res = seek_with(&b, None, 5000).await.unwrap();
        assert_eq!(res["position_ms"], 5000);
    }

    #[tokio::test]
    async fn status_includes_shuffle_loop_rate() {
        let b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 5_000_000, "Playing", Some("/Track/1"));
        let v = status_with(&b, None).await.unwrap();
        assert_eq!(v["shuffle"], false);
        assert_eq!(v["loop_status"], "None");
        assert_eq!(v["rate"], 1.0);
        assert_eq!(v["position_ms"], 5000);
    }

    #[tokio::test]
    async fn metadata_cache_keeps_length() {
        let player = "org.mpris.MediaPlayer2.test_cache";
        let m1 = MediaMetadata { title: Some("Song".into()), length_micros: Some(210_000_000), track_id: Some("/Track/1".into()), ..Default::default() };
        merge_metadata_cached(player, m1);
        let m2 = MediaMetadata { title: Some("Song".into()), length_micros: None, track_id: None, ..Default::default() };
        let merged = merge_metadata_cached(player, m2);
        assert_eq!(merged.length_micros, Some(210_000_000));
        assert_eq!(merged.track_id.as_deref(), Some("/Track/1"));
    }

    // ---- v0.0.3: negative metadata parsing ----

    #[test]
    fn parse_artist_empty_array() {
        let mut m = HashMap::new();
        m.insert("xesam:artist".into(), arr_str(&[]));
        let p = parse_metadata(&m);
        assert!(p.artists.is_empty());
    }

    #[test]
    fn parse_artist_mixed_array_skips_non_strings() {
        let mixed: Vec<Value> = vec![Value::from("Keep"), Value::from(42_u32)];
        let mut m = HashMap::new();
        m.insert(
            "xesam:artist".into(),
            OwnedValue::try_from(Value::new(mixed)).unwrap(),
        );
        let p = parse_metadata(&m);
        assert_eq!(p.artists, vec!["Keep"]);
    }

    #[test]
    fn parse_artist_wrong_type_yields_empty() {
        let mut m = HashMap::new();
        m.insert("xesam:artist".into(), i64v(7));
        let p = parse_metadata(&m);
        assert!(p.artists.is_empty());
    }

    #[test]
    fn parse_title_wrong_type_is_none() {
        let mut m = HashMap::new();
        m.insert("xesam:title".into(), arr_str(&["not", "a", "title"]));
        let p = parse_metadata(&m);
        assert!(p.title.is_none());
    }

    #[test]
    fn parse_length_small_int_variants() {
        for (name, v) in [
            ("i32", OwnedValue::try_from(Value::new(1_234_i32)).unwrap()),
            ("i16", OwnedValue::try_from(Value::new(567_i16)).unwrap()),
            ("u16", OwnedValue::try_from(Value::new(89_u16)).unwrap()),
            ("u8", OwnedValue::try_from(Value::new(12_u8)).unwrap()),
        ] {
            let mut m = HashMap::new();
            m.insert("mpris:length".into(), v);
            let p = parse_metadata(&m);
            assert_eq!(
                p.length_micros,
                Some(match name { "i32" => 1234, "i16" => 567, "u16" => 89, _ => 12 }),
                "variant {name} failed"
            );
        }
    }

    #[test]
    fn parse_length_float_rejected() {
        let mut m = HashMap::new();
        m.insert("mpris:length".into(), OwnedValue::try_from(Value::new(1.5_f64)).unwrap());
        let p = parse_metadata(&m);
        assert!(p.length_micros.is_none());
    }

    #[test]
    fn parse_length_negative_rejected() {
        let mut m = HashMap::new();
        m.insert("mpris:length".into(), i64v(-42));
        let p = parse_metadata(&m);
        assert!(p.length_micros.is_none());
    }

    // ---- v0.0.3: capability guards ----

    #[tokio::test]
    async fn guard_seek_denied_when_can_seek_false() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Playing", Some("/org/mpris/MediaPlayer2/Track/1"));
        b.caps.insert("CanSeek".into(), false);
        let err = seek_with(&b, None, 5000).await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NOT_SUPPORTED"), "{err}");
        assert!(err.contains("CanSeek"), "{err}");
    }

    #[tokio::test]
    async fn guard_pause_denied_when_can_pause_false() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Playing", None);
        b.caps.insert("CanPause".into(), false);
        let err = pause_with(&b, None).await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NOT_SUPPORTED"), "{err}");
    }

    #[tokio::test]
    async fn guard_play_pause_denied_when_can_pause_false() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Paused", None);
        b.caps.insert("CanPause".into(), false);
        let err = play_pause_with(&b, None).await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NOT_SUPPORTED"), "{err}");
    }

    #[tokio::test]
    async fn guard_next_denied_when_can_go_next_false() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Playing", None);
        b.caps.insert("CanGoNext".into(), false);
        let err = next_with(&b, None).await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NOT_SUPPORTED"), "{err}");
    }

    #[tokio::test]
    async fn guard_prev_denied_when_can_go_previous_false() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Playing", None);
        b.caps.insert("CanGoPrevious".into(), false);
        let err = prev_with(&b, None).await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NOT_SUPPORTED"), "{err}");
    }

    #[tokio::test]
    async fn guard_play_denied_when_can_play_false() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Paused", None);
        b.caps.insert("CanPlay".into(), false);
        let err = play_with(&b, None, None).await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NOT_SUPPORTED"), "{err}");
    }

    #[tokio::test]
    async fn guard_volume_denied_when_can_control_false() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Playing", None);
        b.caps.insert("CanControl".into(), false);
        let err = set_volume_with(&b, None, 0.5).await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NOT_SUPPORTED"), "{err}");
    }

    #[tokio::test]
    async fn guard_shuffle_denied_when_can_control_false() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Playing", None);
        b.caps.insert("CanControl".into(), false);
        let err = set_shuffle_with(&b, None, true).await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NOT_SUPPORTED"), "{err}");
    }

    #[tokio::test]
    async fn guard_loop_denied_when_can_control_false() {
        let mut b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Playing", None);
        b.caps.insert("CanControl".into(), false);
        let err = set_loop_with(&b, None, "track").await.unwrap_err();
        assert!(err.contains("ERR_MEDIA_NOT_SUPPORTED"), "{err}");
    }

    #[tokio::test]
    async fn guard_missing_capability_does_not_block() {
        let b = mock(vec!["org.mpris.MediaPlayer2.spotify"], 0, "Paused", None);
        play_with(&b, None, None).await.expect("missing property must not block");
    }

    // ---- v0.0.3: seek_relative ----

    #[tokio::test]
    async fn seek_relative_forward() {
        let b = mock(vec!["org.mpris.MediaPlayer2.mpd"], 6_000_000, "Playing", Some("/org/mpd/Tracks/3"));
        let res = seek_relative_with(&b, None, 25_000).await.unwrap();
        assert_eq!(res["position_ms"], 31_000);
    }

    #[tokio::test]
    async fn seek_relative_backward() {
        let b = mock(vec!["org.mpris.MediaPlayer2.mpd"], 30_000_000, "Playing", Some("/org/mpd/Tracks/3"));
        let res = seek_relative_with(&b, None, -15_000).await.unwrap();
        assert_eq!(res["position_ms"], 15_000);
    }

    #[tokio::test]
    async fn seek_relative_clamps_at_zero() {
        let b = mock(vec!["org.mpris.MediaPlayer2.mpd"], 1_000_000, "Playing", Some("/org/mpd/Tracks/3"));
        let res = seek_relative_with(&b, None, -5_000).await.unwrap();
        assert_eq!(res["position_ms"], 0);
    }

    // ---- v0.0.3: error classification ----

    #[test]
    fn classify_no_such_property_is_not_supported() {
        assert_eq!(classify_dbus("No such property 'Shuffle'"), Some("NOT_SUPPORTED"));
    }

    #[test]
    fn classify_service_unknown_is_vanished() {
        assert_eq!(classify_dbus("ServiceUnknown: The name was not provided"), Some("VANISHED"));
    }

    #[test]
    fn classify_unrecognized_message_is_none() {
        assert_eq!(classify_dbus("some other dbus failure"), None);
    }

    #[test]
    fn wrap_control_err_maps_property_error_to_not_supported() {
        let wrapped = wrap_control_err("p", "set Shuffle", "No such property");
        assert!(wrapped.starts_with("ERR_MEDIA_NOT_SUPPORTED"), "{wrapped}");
    }

    #[test]
    fn wrap_control_err_defaults_to_vanished() {
        let wrapped = wrap_control_err("p", "Pause", "weird failure");
        assert!(wrapped.starts_with("ERR_MEDIA_PLAYER_VANISHED"), "{wrapped}");
    }

    // ---- v0.0.3: signal-fed cache + extrapolation window ----

    #[test]
    fn cache_note_updates_position_and_rate_independently() {
        cache_note("cn_test_player", Some(5_000_000), None);
        cache_note("cn_test_player", None, Some(2.0));
        let entry = pos_cache().lock().unwrap().get("cn_test_player").copied().unwrap();
        assert_eq!(entry.0, 5_000_000);
        assert_eq!(entry.1, 2.0);
    }

    #[test]
    fn extrapolate_uses_signal_fed_sample() {
        let key = "ext_test_fresh";
        pos_cache().lock().unwrap().insert(key.to_string(), (10_000_000, 1.0, Instant::now()));
        let estimated = extrapolate_position(key, 0, 1.0, "Playing");
        assert!(estimated >= 10_000_000, "estimated {estimated}");
    }

    #[test]
    fn extrapolate_stale_sample_not_trusted() {
        let key = "ext_test_stale";
        let stale = Instant::now() - std::time::Duration::from_secs(600);
        pos_cache().lock().unwrap().insert(key.to_string(), (10_000_000, 1.0, stale));
        let result = extrapolate_position(key, 0, 1.0, "Playing");
        assert_eq!(result, 0);
    }
}
