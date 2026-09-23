//! Stream actions: per-application volume / mute / output device, joining
//! MPRIS players (what plays) with audio streams (where it sounds).
//!
//! Target selection, most specific wins: `stream` (id from
//! `media_streams`) > `app` (name substring) > `player` (MPRIS name, joined
//! by PID) > the active player (same resolution as the transport actions).

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::mpris;
use crate::streams::{self, AudioStream, MuteMode, Snapshot, Streams};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Target {
    pub stream: Option<u32>,
    pub app: Option<String>,
    pub player: Option<String>,
}

impl Target {
    pub fn from_params(params: &Value, player: Option<String>) -> Result<Self, String> {
        let stream = match params.get("stream") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or("ERR_MEDIA_BAD_PARAMS: stream must be a stream id (integer) from media_streams")?,
            ),
        };
        let app = match params.get("app") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_str().map(str::trim) {
                Some(s) if !s.is_empty() => Some(s.to_string()),
                _ => return Err("ERR_MEDIA_BAD_PARAMS: app must be a non-empty string".into()),
            },
        };
        Ok(Self { stream, app, player })
    }
}

/// MPRIS player -> its streams, joined by the D-Bus owner PID.
fn player_streams<'a>(snap: &'a Snapshot, player: &str, pid: Option<u32>) -> Vec<&'a AudioStream> {
    streams::streams_for_player(&snap.streams, player, pid, &streams::proc_parent)
}

async fn select<'a>(snap: &'a Snapshot, target: &Target) -> Result<(Vec<&'a AudioStream>, Option<String>), String> {
    if let Some(id) = target.stream {
        let s = snap
            .streams
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("ERR_MEDIA_NO_STREAM: no audio stream with id {id}"))?;
        return Ok((vec![s], None));
    }
    if let Some(app) = &target.app {
        let hits = streams::streams_for_app(&snap.streams, app);
        if hits.is_empty() {
            return Err(format!(
                "ERR_MEDIA_NO_STREAM: no audio stream for app '{app}' (have: {:?})",
                snap.streams.iter().map(|s| s.app.as_str()).collect::<Vec<_>>()
            ));
        }
        return Ok((hits, None));
    }
    let player = match &target.player {
        Some(p) => {
            let available = mpris::list_players().await?;
            crate::resolve::match_requested(p, &available)?
        }
        None => mpris::active_player().await?,
    };
    let pid = mpris::player_pid(&player).await.ok();
    let hits = player_streams(snap, &player, pid);
    if hits.is_empty() {
        return Err(format!(
            "ERR_MEDIA_NO_STREAM: player '{player}' has no audio stream right now (players only get one while outputting sound)"
        ));
    }
    Ok((hits, Some(player)))
}

fn describe(s: &AudioStream) -> Value {
    serde_json::to_value(s).unwrap_or(Value::Null)
}

/// Re-read state after a change so the result reports what the audio
/// server actually applied, not what was asked.
async fn after(st: &Streams, ids: &[u32], player: Option<String>) -> Result<Value, String> {
    let snap = st.snapshot().await?;
    let streams: Vec<Value> = snap.streams.iter().filter(|s| ids.contains(&s.id)).map(describe).collect();
    let mut out = json!({ "ok": true, "streams": streams });
    if let Some(p) = player {
        out["player"] = json!(p);
    }
    Ok(out)
}

pub async fn list(app: Option<&str>) -> Result<Value, String> {
    let snap = Streams::real().snapshot().await?;
    let streams: Vec<&AudioStream> = match app {
        Some(a) => streams::streams_for_app(&snap.streams, a),
        None => snap.streams.iter().collect(),
    };
    // Tag each stream with its MPRIS player so the agent can go from
    // "Spotify stream" to the `player` value transport actions take.
    let mut owners: HashMap<u32, String> = HashMap::new();
    if let Ok(players) = mpris::list_players().await {
        for p in players {
            let pid = mpris::player_pid(&p).await.ok();
            for s in player_streams(&snap, &p, pid) {
                owners.entry(s.id).or_insert_with(|| p.clone());
            }
        }
    }
    let streams: Vec<Value> = streams
        .into_iter()
        .map(|s| {
            let mut v = describe(s);
            v["audible"] = json!(s.audible());
            if let Some(p) = owners.get(&s.id) {
                v["player"] = json!(p);
            }
            v
        })
        .collect();
    Ok(json!({ "backend": snap.backend, "streams": streams, "sinks": snap.sinks }))
}

pub async fn set_volume(target: &Target, level: f64) -> Result<Value, String> {
    let st = Streams::real();
    let snap = st.snapshot().await?;
    let (hits, player) = select(&snap, target).await?;
    let ids: Vec<u32> = hits.iter().map(|s| s.id).collect();
    for id in &ids {
        st.set_volume(snap.backend, *id, level).await?;
    }
    after(&st, &ids, player).await
}

pub async fn set_mute(target: &Target, mode: MuteMode) -> Result<Value, String> {
    let st = Streams::real();
    let snap = st.snapshot().await?;
    let (hits, player) = select(&snap, target).await?;
    let ids: Vec<u32> = hits.iter().map(|s| s.id).collect();
    // Toggle a multi-stream app (browser tabs) as one unit: if any stream
    // is unmuted, mute all; otherwise unmute all. Per-stream toggle would
    // leave a mixed state.
    let mode = match mode {
        MuteMode::Toggle if hits.len() > 1 => {
            if hits.iter().any(|s| !s.muted) { MuteMode::On } else { MuteMode::Off }
        }
        m => m,
    };
    for id in &ids {
        st.set_mute(snap.backend, *id, mode).await?;
    }
    after(&st, &ids, player).await
}

pub async fn move_to(target: &Target, sink: &str) -> Result<Value, String> {
    let st = Streams::real();
    let snap = st.snapshot().await?;
    let sink = streams::find_sink(&snap.sinks, sink)?.clone();
    let (hits, player) = select(&snap, target).await?;
    let ids: Vec<u32> = hits.iter().map(|s| s.id).collect();
    for id in &ids {
        st.move_to(snap.backend, *id, &sink).await?;
    }
    // The graph relinks asynchronously; give it a moment before re-reading.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let mut out = after(&st, &ids, player).await?;
    out["sink"] = json!(sink);
    Ok(out)
}

/// `stream` block for `media_status`: the player's streams, or `null` when
/// it has none / audio tools are unavailable. Never fails the status call.
pub async fn status_streams(player: &str) -> Value {
    let Ok(snap) = Streams::real().snapshot().await else { return Value::Null };
    let pid = mpris::player_pid(player).await.ok();
    let hits = player_streams(&snap, player, pid);
    if hits.is_empty() {
        return Value::Null;
    }
    Value::Array(hits.into_iter().map(describe).collect())
}

/// Audibility per player, for tie-breaking between several `Playing`
/// players. Players without a matching stream are left out.
pub async fn audible_map(players: &[String]) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    let Ok(snap) = Streams::real().snapshot().await else { return out };
    for p in players {
        let pid = mpris::player_pid(p).await.ok();
        let hits = player_streams(&snap, p, pid);
        if !hits.is_empty() {
            out.insert(p.clone(), hits.iter().any(|s| s.audible()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parses_all_selectors() {
        let t = Target::from_params(&json!({"stream": 78, "app": " Firefox "}), Some("spotify".into())).unwrap();
        assert_eq!(t, Target { stream: Some(78), app: Some("Firefox".into()), player: Some("spotify".into()) });
    }

    #[test]
    fn target_rejects_bad_types() {
        assert!(Target::from_params(&json!({"stream": "78"}), None).unwrap_err().starts_with("ERR_MEDIA_BAD_PARAMS"));
        assert!(Target::from_params(&json!({"stream": -1}), None).is_err());
        assert!(Target::from_params(&json!({"app": ""}), None).is_err());
        assert!(Target::from_params(&json!({"app": 3}), None).is_err());
    }

    #[test]
    fn target_empty_is_active_player() {
        assert_eq!(Target::from_params(&json!({"stream": null}), None).unwrap(), Target::default());
    }

    #[tokio::test]
    async fn select_by_stream_and_app_needs_no_dbus() {
        let snap = streams::parse_pw_dump(include_str!("testdata/pw-dump.json")).unwrap();
        let (hits, player) = select(&snap, &Target { stream: Some(78), ..Default::default() }).await.unwrap();
        assert_eq!(hits[0].app, "Spotify");
        assert!(player.is_none());
        let (hits, _) = select(&snap, &Target { app: Some("music player".into()), ..Default::default() }).await.unwrap();
        assert_eq!(hits[0].id, 73);
        let err = select(&snap, &Target { stream: Some(1), ..Default::default() }).await.unwrap_err();
        assert!(err.starts_with("ERR_MEDIA_NO_STREAM"), "{err}");
        let err = select(&snap, &Target { app: Some("vlc".into()), ..Default::default() }).await.unwrap_err();
        assert!(err.contains("Spotify"), "error lists available apps: {err}");
    }
}
