//! `media` plugin — local MPRIS media playback control for vynkor plugins.
//!
//! v1 is local-only: it drives `org.mpris.MediaPlayer2.*` players on the
//! session D-Bus (Spotify, mpv, VLC, browsers, …), so it needs no network
//! permission. Same shape as `ping-pong-rs`: implements the SDK's `Plugin`
//! trait (sequential, one request at a time) and delegates every D-Bus call
//! to the `mpris` module. See ROADMAP.md for the design rationale.

mod audio;
mod bus;
mod manifest;
mod mpris;
mod resolve;
mod runner;
mod streams;

use serde_json::Value;
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, PluginManifest,
};
use vynkor_sdk::{Plugin, VynkorClient, VynkorError};

const PLUGIN_ID: &str = "media";
const PLUGIN_VERSION: &str = "0.0.3";

struct MediaPlugin;

impl Plugin for MediaPlugin {
    fn id(&self) -> &str {
        PLUGIN_ID
    }

    fn version(&self) -> &str {
        PLUGIN_VERSION
    }

    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            actions: vec![
                "media_play".to_string(),
                "media_pause".to_string(),
                "media_play_pause".to_string(),
                "media_next".to_string(),
                "media_prev".to_string(),
                "media_stop".to_string(),
                "media_seek".to_string(),
                "media_seek_relative".to_string(),
                "media_volume".to_string(),
                "media_status".to_string(),
                "media_list_players".to_string(),
                "media_shuffle".to_string(),
                "media_loop".to_string(),
                "media_raise".to_string(),
                "media_quit".to_string(),
                "media_streams".to_string(),
                "media_stream_volume".to_string(),
                "media_stream_mute".to_string(),
                "media_stream_move".to_string(),
            ],
            action_specs: manifest::action_specs(),
            ..Default::default()
        }
    }

    async fn on_init(&mut self, _client: &mut VynkorClient) -> Result<(), VynkorError> {
        mpris::spawn_watch_task();
        Ok(())
    }

    async fn on_message(&mut self, envelope: Envelope) -> Result<Option<Envelope>, VynkorError> {
        match envelope.payload {
            Some(envelope::Payload::ActionRequest(req)) => {
                let response = handle_action_request(req).await;
                Ok(Some(Envelope {
                    payload: Some(envelope::Payload::ActionResponse(response)),
                    ..Default::default()
                }))
            }
            _ => Ok(None),
        }
    }

    async fn on_shutdown(&mut self) -> Result<(), VynkorError> {
        Ok(())
    }
}

/// Parse the request params, extract the optional `player` selector, and
/// dispatch to the matching `mpris` stub. Every path returns an
/// [`ActionResponse`]; errors map to `ACTION_ERROR`, unknown actions to
/// `ACTION_NOT_FOUND`.
async fn handle_action_request(req: ActionRequest) -> ActionResponse {
    let params: Value = match serde_json::from_slice(&req.params_json) {
        Ok(v) => v,
        Err(e) => {
            return ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionError as i32,
                data_json: Vec::new(),
                error: format!("invalid params_json: {e}"),
            };
        }
    };

    let player: Option<String> = match params.get("player") {
        None => None,
        Some(Value::Null) => None,
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => Some(s.to_string()),
            Some(_) => {
                return ActionResponse {
                    action_id: req.action_id,
                    status: ActionStatus::ActionError as i32,
                    data_json: Vec::new(),
                    error: "ERR_MEDIA_BAD_PARAMS: player must be non-empty string".to_string(),
                };
            }
            None => {
                return ActionResponse {
                    action_id: req.action_id,
                    status: ActionStatus::ActionError as i32,
                    data_json: Vec::new(),
                    error: "ERR_MEDIA_BAD_PARAMS: player must be string".to_string(),
                };
            }
        },
    };

    let result: Result<Value, String> = match req.action.as_str() {
        "media_play" => {
            let uri = match params.get("uri") {
                None | Some(Value::Null) => None,
                Some(v) => match v.as_str() {
                    Some(s) => Some(s.to_string()),
                    None => {
                        return ActionResponse {
                            action_id: req.action_id,
                            status: ActionStatus::ActionError as i32,
                            data_json: Vec::new(),
                            error: "ERR_MEDIA_BAD_PARAMS: uri must be string".to_string(),
                        };
                    }
                },
            };
            mpris::play(player.as_deref(), uri.as_deref()).await
        }
        "media_pause" => mpris::pause(player.as_deref()).await,
        "media_play_pause" => mpris::play_pause(player.as_deref()).await,
        "media_next" => mpris::next(player.as_deref()).await,
        "media_prev" => mpris::prev(player.as_deref()).await,
        "media_stop" => mpris::stop(player.as_deref()).await,
        "media_seek" => match params.get("position_ms").and_then(Value::as_u64) {
            Some(position_ms) => mpris::seek(player.as_deref(), position_ms).await,
            None => Err("ERR_MEDIA_BAD_PARAMS: missing or invalid `position_ms`".to_string()),
        },
        "media_seek_relative" => match params.get("offset_ms").and_then(Value::as_i64) {
            Some(offset_ms) => mpris::seek_relative(player.as_deref(), offset_ms).await,
            None => Err("ERR_MEDIA_BAD_PARAMS: missing or invalid `offset_ms` (integer, ms; may be negative)".to_string()),
        },
        "media_volume" => match parse_volume(&params) {
            Some(level) => mpris::set_volume(player.as_deref(), level).await,
            None => Err("ERR_MEDIA_BAD_PARAMS: missing or invalid `level`".to_string()),
        },
        "media_status" => match mpris::status(player.as_deref()).await {
            Ok(mut v) => {
                if let Some(p) = v["player"].as_str().map(str::to_string) {
                    v["stream"] = audio::status_streams(&p).await;
                }
                Ok(v)
            }
            Err(e) => Err(e),
        },
        "media_raise" => mpris::raise(player.as_deref()).await,
        "media_quit" => mpris::quit(player.as_deref()).await,
        "media_streams" => match params.get("app") {
            None | Some(Value::Null) => audio::list(None).await,
            Some(v) => match v.as_str() {
                Some(s) => audio::list(Some(s)).await,
                None => Err("ERR_MEDIA_BAD_PARAMS: app must be string".to_string()),
            },
        },
        "media_stream_volume" | "media_stream_mute" | "media_stream_move" => {
            match audio::Target::from_params(&params, player.clone()) {
                Err(e) => Err(e),
                Ok(target) => match req.action.as_str() {
                    "media_stream_volume" => match parse_volume(&params) {
                        Some(level) => audio::set_volume(&target, level).await,
                        None => Err("ERR_MEDIA_BAD_PARAMS: missing or invalid `level`".to_string()),
                    },
                    "media_stream_mute" => {
                        match params.get("mode").and_then(Value::as_str).and_then(streams::MuteMode::parse) {
                            Some(mode) => audio::set_mute(&target, mode).await,
                            None => Err("ERR_MEDIA_BAD_PARAMS: missing or invalid `mode` (on/off/toggle)".to_string()),
                        }
                    }
                    _ => match params.get("sink").and_then(Value::as_str).map(str::trim) {
                        Some(sink) if !sink.is_empty() => audio::move_to(&target, sink).await,
                        _ => Err("ERR_MEDIA_BAD_PARAMS: missing `sink` (id, name or description from media_streams)".to_string()),
                    },
                },
            }
        }
        "media_list_players" => mpris::list_players()
            .await
            .map(|players| serde_json::json!({ "players": players })),
        "media_shuffle" => {
            let enabled = params.get("enabled").and_then(Value::as_bool).or_else(|| {
                params.get("shuffle").and_then(Value::as_bool)
            });
            match enabled {
                Some(v) => mpris::set_shuffle(player.as_deref(), v).await,
                None => Err("ERR_MEDIA_BAD_PARAMS: missing or invalid `enabled` (bool)".to_string()),
            }
        }
        "media_loop" => {
            let mode = params.get("mode").or_else(|| params.get("loop_status")).or_else(|| params.get("loop"));
            match mode.and_then(Value::as_str) {
                Some(s) => mpris::set_loop(player.as_deref(), s).await,
                None => Err("ERR_MEDIA_BAD_PARAMS: missing or invalid `mode` (none/track/playlist)".to_string()),
            }
        }
        other => {
            return ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionNotFound as i32,
                data_json: Vec::new(),
                error: format!("unknown action: {other}"),
            };
        }
    };

    match result {
        Ok(data) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionOk as i32,
            data_json: data.to_string().into_bytes(),
            error: String::new(),
        },
        Err(error) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionError as i32,
            data_json: Vec::new(),
            error,
        },
    }
}

/// Normalize a `media_volume` `level` to a `0.0..=1.0` fraction. Accepts a
/// fractional `0.0`..=`1.0` value or a `0`..=`100` integer percentage. JSON
/// `1` (integer) reads as 1% while `1.0` (float) reads as 100% — an
/// inherent ambiguity of "fraction or percentage" that callers resolve by
/// sending floats for fractions and integers for percentages.
/// Integers are detected via `is_u64/is_i64` before `as_f64` so that `1`
/// does not collapse to `1.0`.
fn parse_volume(params: &Value) -> Option<f64> {
    let level = params.get("level")?;
    // Integer path — percentages 0..100 (1 => 0.01). Must be checked before
    // as_f64() because serde_json::Value::as_f64() returns Some for integers.
    if level.is_u64() {
        if let Some(n) = level.as_u64() {
            if n <= 100 {
                return Some(n as f64 / 100.0);
            }
            return None;
        }
    }
    if level.is_i64() {
        if let Some(n) = level.as_i64() {
            if (0..=100).contains(&n) {
                return Some(n as f64 / 100.0);
            }
            return None;
        }
    }
    if let Some(n) = level.as_f64() {
        if (0.0..=1.0).contains(&n) {
            return Some(n);
        }
        if (1.0..=100.0).contains(&n) {
            return Some(n / 100.0);
        }
        return None;
    }
    None
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    MediaPlugin.run().await
}
