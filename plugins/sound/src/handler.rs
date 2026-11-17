//! Action handlers for the `sound` plugin: `sound_play`, `sound_stop`,
//! `sound_status`. All process spawning goes through
//! [`crate::players::Spawner`] so tests run without real audio hardware.
//!
//! Playback model: `sound_play` returns as soon as the player process is
//! spawned — clips play in the background. The plugin is the *single owner
//! of the speakers*: starting a new clip stops whatever was playing, and
//! stopping is idempotent. Temp files written for inline audio are removed
//! on the next handler interaction after the clip finishes (reap), and
//! best-effort at shutdown.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};

use crate::players::{build_args, player_chain, BoxedProcess, Config, Spawner};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One playing clip: its process handle plus everything status reports.
pub struct Clip {
    pub proc: BoxedProcess,
    pub meta: ClipMeta,
}

#[derive(Debug, Clone)]
pub struct ClipMeta {
    pub id: String,
    /// What is being played: file path or `inline/<format>`.
    pub source: String,
    /// Backend binary that was spawned.
    pub player_bin: String,
    /// Temp file to remove once this clip finishes (inline audio only).
    pub temp_path: Option<PathBuf>,
}

/// Shared plugin state: monotonically increasing clip ids + active clips.
#[derive(Default)]
pub struct State {
    next_id: u64,
    clips: HashMap<String, Clip>,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }
}

pub type SharedState = std::sync::Arc<Mutex<State>>;

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

/// Reap finished clips: drop entries whose process has exited and delete
/// their temp files. Called at the top of every action so `sound_status`
/// converges to empty without any background watcher task.
fn reap_finished(state: &SharedState) {
    let mut st = state.lock().unwrap();
    reap_finished_locked(&mut st);
}

fn reap_finished_locked(st: &mut State) {
    let mut finished: Vec<String> = Vec::new();
    for (id, clip) in st.clips.iter_mut() {
        if clip.proc.try_wait().is_some() {
            finished.push(id.clone());
        }
    }
    for id in finished {
        if let Some(mut clip) = st.clips.remove(&id) {
            if let Some(tmp) = clip.meta.temp_path.take() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Kill every active clip; returns their ids in insertion order. Used by
/// replace-on-play and by shutdown.
pub fn kill_all(state: &SharedState) -> Vec<String> {
    let mut st = state.lock().unwrap();
    kill_all_locked(&mut st)
}

fn kill_all_locked(st: &mut State) -> Vec<String> {
    let ids: Vec<String> = st.clips.keys().cloned().collect();
    for (_, mut clip) in st.clips.drain() {
        clip.proc.start_kill();
    }
    // Dropping the BoxedProcess reaps killed children (kill_on_drop).
    ids
}

// ---------------------------------------------------------------------------
// Request parsing (strict — serde validates types, not shape)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Source {
    File(String),
    Inline { data_base64: String, format: String },
}

#[derive(Debug, Clone)]
pub struct PlayRequest {
    pub source: Source,
    /// Linear multiplier, 1.0 = unchanged.
    pub volume: f64,
    pub device: Option<String>,
}

impl PlayRequest {
    /// Strict parse with shape-naming errors. `file` must be absolute (the
    /// plugin's CWD is kernel-dependent, so relative paths are ambiguous);
    /// exactly one of `file` / `data_base64`; `format` is required with
    /// inline audio and must be a short alphanumeric extension.
    pub fn parse(params: &Value) -> Result<Self, String> {
        let obj = params
            .as_object()
            .ok_or_else(|| "ERR_SOUND_BAD_PARAMS: params must be a JSON object".to_string())?;

        let file = obj.get("file").map(|v| {
            v.as_str()
                .ok_or_else(|| "ERR_SOUND_BAD_PARAMS: 'file' must be a string".to_string())
        });
        let data = obj.get("data_base64").map(|v| {
            v.as_str()
                .ok_or_else(|| "ERR_SOUND_BAD_PARAMS: 'data_base64' must be a string".to_string())
        });

        let source = match (file, data) {
            (Some(f), None) => {
                let f = f?;
                if f.trim().is_empty() {
                    return Err("ERR_SOUND_BAD_PARAMS: 'file' must be non-empty".to_string());
                }
                if !f.starts_with('/') {
                    return Err(format!(
                        "ERR_SOUND_BAD_PARAMS: 'file' must be an absolute path, got '{f}'"
                    ));
                }
                Source::File(f.to_string())
            }
            (None, Some(d)) => {
                let d = d?;
                if d.trim().is_empty() {
                    return Err("ERR_SOUND_BAD_PARAMS: 'data_base64' must be non-empty".to_string());
                }
                let format = obj.get("format").and_then(Value::as_str).ok_or_else(|| {
                    "ERR_SOUND_BAD_PARAMS: 'format' is required with 'data_base64' \
                     (e.g. \"wav\")"
                        .to_string()
                })?;
                validate_format(format)?;
                Source::Inline {
                    data_base64: d.to_string(),
                    format: format.to_ascii_lowercase(),
                }
            }
            (Some(_), Some(_)) => {
                return Err("ERR_SOUND_BAD_PARAMS: pass exactly one of 'file' or \
                            'data_base64', not both"
                    .to_string());
            }
            (None, None) => {
                return Err("ERR_SOUND_BAD_PARAMS: one of 'file' or 'data_base64' \
                            is required"
                    .to_string());
            }
        };

        let volume = match obj.get("volume") {
            None | Some(Value::Null) => 1.0,
            Some(v) => {
                let n = v
                    .as_f64()
                    .ok_or_else(|| "ERR_SOUND_BAD_PARAMS: 'volume' must be a number".to_string())?;
                if !n.is_finite() || !(0.0..=10.0).contains(&n) {
                    return Err(format!(
                        "ERR_SOUND_BAD_PARAMS: 'volume' must be within [0, 10], got {n}"
                    ));
                }
                n
            }
        };

        let device = match obj.get("device") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| "ERR_SOUND_BAD_PARAMS: 'device' must be a string".to_string())?;
                let s = s.trim();
                if s.is_empty() {
                    return Err("ERR_SOUND_BAD_PARAMS: 'device' must be non-empty".to_string());
                }
                Some(s.to_string())
            }
        };

        Ok(Self {
            source,
            volume,
            device,
        })
    }
}

fn validate_format(format: &str) -> Result<(), String> {
    if format.is_empty() || format.len() > 8 || !format.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "ERR_SOUND_BAD_PARAMS: invalid 'format' '{format}' (expected short \
             alphanumeric like wav/mp3/ogg/flac)"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn handle_play(
    spawner: &dyn Spawner,
    cfg: &Config,
    state: &SharedState,
    req: &PlayRequest,
) -> Result<Value, String> {
    reap_finished(state);

    // Resolve the concrete file to hand to the player.
    let (source_path, temp_path, source_desc, format_is_wav) = match &req.source {
        Source::File(path) => {
            let meta = std::fs::metadata(path)
                .map_err(|e| format!("ERR_SOUND_SOURCE_UNREADABLE: cannot access '{path}': {e}"))?;
            if meta.len() as usize > cfg.max_bytes {
                return Err(format!(
                    "ERR_SOUND_TOO_LARGE: {} bytes exceeds SOUND_PLUGIN_MAX_BYTES={}",
                    meta.len(),
                    cfg.max_bytes
                ));
            }
            let ext = path.rsplit('.').next().unwrap_or("");
            let wav = ext.eq_ignore_ascii_case("wav");
            (path.clone(), None, path.clone(), wav)
        }
        Source::Inline {
            data_base64,
            format,
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_base64.as_bytes())
                .map_err(|e| format!("ERR_SOUND_BAD_PARAMS: invalid base64 audio: {e}"))?;
            if bytes.len() > cfg.max_bytes {
                return Err(format!(
                    "ERR_SOUND_TOO_LARGE: {} decoded bytes exceeds \
                     SOUND_PLUGIN_MAX_BYTES={}",
                    bytes.len(),
                    cfg.max_bytes
                ));
            }
            let tmp = cfg.temp_dir.join(format!(
                "sound-{}-{}.{format}",
                std::process::id(),
                unix_millis()
            ));
            std::fs::write(&tmp, &bytes).map_err(|e| {
                format!("ERR_SOUND_INTERNAL: failed to write {}: {e}", tmp.display())
            })?;
            (
                tmp.to_string_lossy().into_owned(),
                Some(tmp),
                format!("inline/{format}"),
                format == "wav",
            )
        }
    };

    // Pick the backend chain and spawn the first working candidate.
    let chain = player_chain(
        format_is_wav,
        req.volume,
        req.device.as_deref(),
        cfg.player_override.as_deref(),
    );
    let chain = match chain {
        Ok(c) => c,
        Err(e) => {
            cleanup_temp(temp_path.as_deref());
            return Err(e);
        }
    };

    let device = req.device.as_deref().or(cfg.default_device.as_deref());
    let mut tried: Vec<String> = Vec::new();
    let mut spawned: Option<(String, BoxedProcess)> = None;
    for bin in &chain {
        let args = build_args(bin, &source_path, req.volume, device);
        match spawner.spawn(bin, &args).await {
            Ok(proc) => {
                spawned = Some((bin.clone(), proc));
                break;
            }
            Err(e) if e.contains("ERR_SOUND_PROVIDER_MISSING") => tried.push(bin.clone()),
            Err(e) => {
                cleanup_temp(temp_path.as_deref());
                return Err(e);
            }
        }
    }

    let (player_bin, proc) = match spawned {
        Some(pair) => pair,
        None => {
            cleanup_temp(temp_path.as_deref());
            return Err(format!(
                "ERR_SOUND_PROVIDER_MISSING: no working audio player found \
                 (tried: {}); install pw-cat (pipewire), paplay (libpulse), \
                 aplay (alsa-utils) or ffplay (ffmpeg), or pin \
                 SOUND_PLUGIN_PLAYER",
                tried.join(", ")
            ));
        }
    };

    // Single owner of the speakers: stop whatever was playing first.
    let replaced_ids = kill_all(state);

    let mut st = state.lock().unwrap();
    st.next_id += 1;
    let id = format!("clip-{}", st.next_id);
    st.clips.insert(
        id.clone(),
        Clip {
            proc,
            meta: ClipMeta {
                id: id.clone(),
                source: source_desc,
                player_bin: player_bin.clone(),
                temp_path,
            },
        },
    );

    Ok(json!({
        "ok": true,
        "clip_id": id,
        "player": player_bin,
        "replaced": !replaced_ids.is_empty(),
    }))
}

/// Stop one specific clip, or everything when `clip_id` is None.
/// Idempotent: unknown / already-finished ids yield an empty list.
pub fn handle_stop(state: &SharedState, clip_id: Option<&str>) -> Value {
    reap_finished(state);

    let mut st = state.lock().unwrap();
    let stopped: Vec<String> = match clip_id {
        Some(id) => match st.clips.remove(id) {
            Some(mut clip) => {
                clip.proc.start_kill();
                vec![id.to_string()]
            }
            None => Vec::new(),
        },
        None => kill_all_locked(&mut st),
    };
    json!({ "stopped": stopped })
}

pub fn handle_status(state: &SharedState) -> Value {
    reap_finished(state);

    let st = state.lock().unwrap();
    let mut clips: Vec<&ClipMeta> = st.clips.values().map(|c| &c.meta).collect();
    clips.sort_by(|a, b| a.id.cmp(&b.id));
    let playing: Vec<Value> = clips
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "source": m.source,
                "player": m.player_bin,
            })
        })
        .collect();
    let count = playing.len();
    json!({ "playing": playing, "count": count })
}

fn cleanup_temp(temp_path: Option<&std::path::Path>) {
    if let Some(p) = temp_path {
        let _ = std::fs::remove_file(p);
    }
}

pub async fn handle_devices() -> Result<serde_json::Value, String> {
    let sinks = match try_pactl().await {
        Ok(v) => v,
        Err(_) => match try_wpctl().await {
            Ok(v) => v,
            Err(_) => Vec::new(),
        },
    };
    let provider = if sinks.is_empty() {
        "none"
    } else {
        "pactl/wpctl"
    };
    Ok(serde_json::json!({"sinks": sinks, "provider": provider}))
}

async fn try_pactl() -> Result<Vec<serde_json::Value>, String> {
    use tokio::process::Command;
    let out = tokio::time::timeout(std::time::Duration::from_millis(2000), async {
        Command::new("pactl")
            .args(["list", "sinks", "short"])
            .output()
            .await
            .map_err(|e| format!("pactl spawn: {e}"))
    })
    .await
    .map_err(|_| "pactl timeout".to_string())??;
    if !out.status.success() {
        return Err(format!(
            "pactl failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut sinks = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let name = parts[1].to_string();
            sinks.push(serde_json::json!({"name": name, "description": name.clone(), "state": parts.get(0).unwrap_or(&"").to_string()}));
        }
    }
    if let Ok(long) = tokio::time::timeout(std::time::Duration::from_millis(2000), async {
        Command::new("pactl")
            .args(["list", "sinks"])
            .output()
            .await
            .map_err(|e| format!("pactl spawn: {e}"))
    })
    .await
    {
        if let Ok(out) = long {
            if out.status.success() {
                let txt = String::from_utf8_lossy(&out.stdout).to_string();
                let mut map = std::collections::HashMap::new();
                let mut cur_name: Option<String> = None;
                for line in txt.lines() {
                    let line = line.trim();
                    if line.starts_with("Name:") {
                        cur_name = Some(line["Name:".len()..].trim().to_string());
                    } else if line.starts_with("Description:") {
                        if let Some(name) = cur_name.take() {
                            let desc = line["Description:".len()..].trim().to_string();
                            map.insert(name, desc);
                        }
                    }
                }
                for sink in &mut sinks {
                    if let Some(name) = sink
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                    {
                        if let Some(desc) = map.get(&name) {
                            sink["description"] = serde_json::Value::String(desc.clone());
                        }
                    }
                }
            }
        }
    }
    Ok(sinks)
}

async fn try_wpctl() -> Result<Vec<serde_json::Value>, String> {
    use tokio::process::Command;
    let out = tokio::time::timeout(std::time::Duration::from_millis(2000), async {
        Command::new("wpctl")
            .arg("status")
            .output()
            .await
            .map_err(|e| format!("wpctl spawn: {e}"))
    })
    .await
    .map_err(|_| "wpctl timeout".to_string())??;
    if !out.status.success() {
        return Err(format!("wpctl failed"));
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut sinks = Vec::new();
    let mut in_sinks = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Sinks:") {
            in_sinks = true;
            continue;
        }
        if in_sinks {
            if trimmed.starts_with("Sources:") || trimmed.starts_with("Streams:") {
                break;
            }
            if trimmed.contains(".") && trimmed.contains("[vol:") {
                if let Some(dot) = trimmed.find('.') {
                    let rest = trimmed[dot + 1..].trim();
                    if let Some(bracket) = rest.find(' ') {
                        let name = rest[..bracket].trim().to_string();
                        sinks.push(serde_json::json!({"name": name, "description": name.clone(), "state": "RUNNING"}));
                    } else {
                        let name = rest.trim().to_string();
                        if !name.is_empty() {
                            sinks.push(serde_json::json!({"name": name, "description": name.clone(), "state": "RUNNING"}));
                        }
                    }
                }
            }
        }
    }
    if sinks.is_empty() {
        return Err("wpctl no sinks".into());
    }
    Ok(sinks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::players::FakeSpawner;
    use std::sync::Arc;

    fn state() -> SharedState {
        Arc::new(Mutex::new(State::new()))
    }

    fn cfg() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            max_bytes: 1024,
            player_override: None,
            default_device: None,
            temp_dir: dir.path().to_path_buf(),
        };
        (cfg, dir)
    }

    const SILENT_WAV_B64: &str = "UklGRiQAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA=";

    #[tokio::test]
    async fn play_inline_spawns_pwcat_and_registers_clip() {
        let (cfg, dir) = cfg();
        let sp = FakeSpawner::ok(None);
        let st = state();

        let req = PlayRequest::parse(&serde_json::json!({
            "data_base64": SILENT_WAV_B64,
            "format": "wav"
        }))
        .unwrap();
        let v = handle_play(&sp, &cfg, &st, &req).await.unwrap();

        assert_eq!(v["ok"], true);
        assert_eq!(v["clip_id"], "clip-1");
        assert_eq!(v["player"], "pw-cat");
        assert_eq!(v["replaced"], false);

        let calls = sp.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "pw-cat");
        // File arg points into our temp dir and exists while spawning.
        let file_arg = calls[0].1.last().unwrap();
        assert!(PathBuf::from(file_arg).starts_with(dir.path()));
        assert!(std::path::Path::new(file_arg).exists());

        let status = handle_status(&st);
        assert_eq!(status["count"], 1);
        assert_eq!(status["playing"][0]["id"], "clip-1");
        assert_eq!(status["playing"][0]["player"], "pw-cat");
    }

    #[tokio::test]
    async fn non_wav_inline_goes_to_ffplay() {
        let (cfg, _dir) = cfg();
        let sp = FakeSpawner::ok(None);
        let st = state();
        let req = PlayRequest::parse(&serde_json::json!({
            "data_base64": SILENT_WAV_B64,
            "format": "mp3"
        }))
        .unwrap();
        let v = handle_play(&sp, &cfg, &st, &req).await.unwrap();
        assert_eq!(v["player"], "ffplay");
        assert!(sp.calls()[0].1.contains(&"-nodisp".to_string()));
    }

    #[tokio::test]
    async fn play_replaces_existing_clip_and_reports_it() {
        let (cfg, _dir) = cfg();
        let sp = FakeSpawner::ok(None);
        let st = state();
        let mk = |fmt: &str| {
            PlayRequest::parse(&serde_json::json!({
                "data_base64": SILENT_WAV_B64, "format": fmt
            }))
            .unwrap()
        };

        handle_play(&sp, &cfg, &st, &mk("wav")).await.unwrap();
        let v2 = handle_play(&sp, &cfg, &st, &mk("wav")).await.unwrap();

        assert_eq!(v2["clip_id"], "clip-2");
        assert_eq!(v2["replaced"], true);
        // First backend was killed exactly once.
        let kills = sp.killed_bins();
        assert_eq!(kills.len(), 1, "{kills:?}");

        let status = handle_status(&st);
        assert_eq!(status["count"], 1);
        assert_eq!(status["playing"][0]["id"], "clip-2");
    }

    #[tokio::test]
    async fn stop_by_id_and_unknown_id() {
        let (cfg, _dir) = cfg();
        let sp = FakeSpawner::ok(None);
        let st = state();
        let req = PlayRequest::parse(&serde_json::json!({
            "data_base64": SILENT_WAV_B64, "format": "wav"
        }))
        .unwrap();
        handle_play(&sp, &cfg, &st, &req).await.unwrap();

        let v = handle_stop(&st, Some("clip-999"));
        assert_eq!(v["stopped"], serde_json::json!([]));

        let v = handle_stop(&st, Some("clip-1"));
        assert_eq!(v["stopped"], serde_json::json!(["clip-1"]));
        assert_eq!(handle_status(&st)["count"], 0);

        // Idempotent: stopping again matches nothing.
        let v = handle_stop(&st, Some("clip-1"));
        assert_eq!(v["stopped"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn natural_finish_reaps_clip_and_removes_temp() {
        let (cfg, _dir) = cfg();
        // auto_exit_ms=30 → the fake process exits on its own shortly after.
        let sp = FakeSpawner::ok(Some(30));
        let st = state();
        let req = PlayRequest::parse(&serde_json::json!({
            "data_base64": SILENT_WAV_B64, "format": "wav"
        }))
        .unwrap();
        handle_play(&sp, &cfg, &st, &req).await.unwrap();

        // Temp file existed right after spawn.
        let temp_file = {
            let guard = st.lock().unwrap();
            guard
                .clips
                .values()
                .next()
                .unwrap()
                .meta
                .temp_path
                .clone()
                .unwrap()
        };
        assert!(temp_file.exists());

        // Wait past the natural exit, then any interaction reaps.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let status = handle_status(&st);
        assert_eq!(status["count"], 0);
        assert!(!temp_file.exists(), "temp file must be cleaned up");
    }

    #[tokio::test]
    async fn inline_oversized_rejected_before_spawn() {
        let (mut cfg, _dir) = cfg();
        cfg.max_bytes = 16;
        let sp = FakeSpawner::ok(None);
        let st = state();

        let big = general_b64(&[0u8; 64]);
        let err = handle_play(
            &sp,
            &cfg,
            &st,
            &PlayRequest::parse(&serde_json::json!({
                "data_base64": big, "format": "wav"
            }))
            .unwrap(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("ERR_SOUND_TOO_LARGE"), "{err}");
        assert!(sp.calls().is_empty(), "must not spawn");
        // No stray temp files.
        assert_eq!(std::fs::read_dir(&cfg.temp_dir).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn missing_providers_fall_through_and_list_tried() {
        let (cfg, _dir) = cfg();
        let missing = |b: &str| {
            Err(format!(
                "ERR_SOUND_PROVIDER_MISSING: binary '{b}' not found on PATH"
            ))
        };
        let sp = FakeSpawner::new(
            vec![("pw-cat", missing("pw-cat")), ("paplay", missing("paplay"))],
            None,
        );
        let st = state();

        let req = PlayRequest::parse(&serde_json::json!({
            "data_base64": SILENT_WAV_B64, "format": "wav"
        }))
        .unwrap();

        // All missing → error names every candidate and cleans the temp file.
        let sp_all_missing = FakeSpawner::new(
            vec![
                ("pw-cat", missing("pw-cat")),
                ("paplay", missing("paplay")),
                ("aplay", missing("aplay")),
                ("ffplay", missing("ffplay")),
            ],
            None,
        );
        let err = handle_play(&sp_all_missing, &cfg, &st, &req)
            .await
            .unwrap_err();
        assert!(err.contains("ERR_SOUND_PROVIDER_MISSING"), "{err}");
        for bin in ["pw-cat", "paplay", "aplay"] {
            assert!(err.contains(bin), "{err}");
        }
        assert_eq!(std::fs::read_dir(&cfg.temp_dir).unwrap().count(), 0);

        // pw-cat/paplay missing → falls through to aplay.
        let v = handle_play(&sp, &cfg, &st, &req).await.unwrap();
        assert_eq!(v["player"], "aplay", "falls through to the next backend");
        assert_eq!(handle_status(&st)["count"], 1);
    }

    #[tokio::test]
    async fn hard_spawn_failure_propagates_immediately() {
        let (cfg, _dir) = cfg();
        let sp = FakeSpawner::new(
            vec![("pw-cat", Err("ERR_SOUND_SPAWN_FAILED: boom".to_string()))],
            None,
        );
        let st = state();
        let req = PlayRequest::parse(&serde_json::json!({
            "data_base64": SILENT_WAV_B64, "format": "wav"
        }))
        .unwrap();
        let err = handle_play(&sp, &cfg, &st, &req).await.unwrap_err();
        assert!(err.contains("ERR_SOUND_SPAWN_FAILED"), "{err}");
        assert_eq!(sp.calls().len(), 1, "no fallthrough on non-missing errors");
    }

    #[tokio::test]
    async fn file_source_checks_existence_size_and_absolute() {
        let (cfg, dir) = cfg();
        let sp = FakeSpawner::ok(None);
        let st = state();

        // Relative path rejected at parse time.
        let err = PlayRequest::parse(&serde_json::json!({ "file": "rel/a.wav" })).unwrap_err();
        assert!(err.contains("absolute"), "{err}");

        // Missing file → SOURCE_UNREADABLE, nothing spawned.
        let err = handle_play(
            &sp,
            &cfg,
            &st,
            &PlayRequest::parse(&serde_json::json!({ "file": "/nonexistent/x.wav" })).unwrap(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("ERR_SOUND_SOURCE_UNREADABLE"), "{err}");
        assert!(sp.calls().is_empty());

        // Oversized file → TOO_LARGE before spawn.
        let big = dir.path().join("big.wav");
        std::fs::write(&big, vec![0u8; 2048]).unwrap();
        let err = handle_play(
            &sp,
            &cfg,
            &st,
            &PlayRequest::parse(&serde_json::json!({
                "file": big.to_string_lossy()
            }))
            .unwrap(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("ERR_SOUND_TOO_LARGE"), "{err}");

        // Good file plays.
        let good = dir.path().join("ok.wav");
        std::fs::write(&good, b"RIFF").unwrap();
        let v = handle_play(
            &sp,
            &cfg,
            &st,
            &PlayRequest::parse(&serde_json::json!({ "file": good.to_string_lossy() })).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(v["player"], "pw-cat");
        assert_eq!(handle_status(&st)["count"], 1);
    }

    #[test]
    fn parse_validation_matrix() {
        let ok = |p: Value| PlayRequest::parse(&p).is_ok();
        let err = |p: Value| PlayRequest::parse(&p).unwrap_err();

        assert!(err(serde_json::json!(null)).contains("object"));
        assert!(err(serde_json::json!({})).contains("one of"));
        assert!(err(serde_json::json!({
            "file": "/a.wav", "data_base64": "x", "format": "wav"
        }))
        .contains("exactly one"));
        assert!(err(serde_json::json!({"data_base64": "x"})).contains("'format'"));
        assert!(
            err(serde_json::json!({"data_base64": "x", "format": "weird/ext"})).contains("format")
        );
        assert!(err(serde_json::json!({"data_base64": "x", "format": ""})).contains("format"));
        assert!(err(serde_json::json!({"file": ""})).contains("non-empty"));
        assert!(err(serde_json::json!({"volume": 11, "file": "/a.wav"})).contains("[0, 10]"));
        assert!(err(serde_json::json!({"device": "", "file": "/a.wav"})).contains("device"));

        assert!(ok(serde_json::json!({"file": "/a.wav"})));
        assert!(ok(serde_json::json!({
            "data_base64": "x", "format": "WAV", "volume": 0, "device": "sink"
        })));

        let r = PlayRequest::parse(&serde_json::json!({"file": "/a.wav"})).unwrap();
        assert_eq!(r.volume, 1.0);
        let r = PlayRequest::parse(&serde_json::json!({
            "file": "/a.wav", "volume": null
        }))
        .unwrap();
        assert_eq!(r.volume, 1.0);
    }

    fn general_b64<const N: usize>(bytes: &[u8; N]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}
