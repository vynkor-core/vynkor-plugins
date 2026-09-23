//! Per-application audio streams — the "where does the sound go" half of
//! media control, next to MPRIS's "what is playing".
//!
//! Two host-tool backends, both argv-only through [`CommandRunner`]:
//!
//! - **PipeWire** (primary): `pw-dump` (JSON graph) to list, `wpctl` for
//!   volume/mute by node id, `pw-metadata <id> target.object <sink>` to
//!   move. `pw-dump`/`pw-metadata` ship in the `pipewire` package and
//!   `wpctl` in `wireplumber`, so nothing extra is needed wherever PipeWire
//!   runs.
//! - **PulseAudio** (fallback when `pw-dump` is absent): `pactl -f json`.
//!
//! Volumes are exposed on the same cubic 0.0-1.0 scale `wpctl`, `pactl`'s
//! percentages and desktop mixers use. `pw-dump` reports linear channel
//! volumes, so they are converted with a cube root.
//!
//! Streams are joined to MPRIS players by PID: PipeWire's client carries
//! `pipewire.sec.pid`, D-Bus gives the MPRIS owner's PID. Browsers play
//! audio from a child process, so the stream PID's ancestors are walked too;
//! a name heuristic covers backends that report no PID.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::runner::{CommandRunner, RealRunner, RunnerError};

const STREAM_CLASS: &str = "Stream/Output/Audio";
const SINK_CLASS: &str = "Audio/Sink";
/// How many parent hops to follow from a stream PID looking for the MPRIS
/// owner (browser content process -> main process is 1-3 hops).
const MAX_PID_HOPS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Pipewire,
    Pulse,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioStream {
    /// PipeWire node id, or PulseAudio sink-input index.
    pub id: u32,
    pub app: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Cubic 0.0-1.0 (can exceed 1.0 when boosted).
    pub volume: f64,
    pub muted: bool,
    /// Actually pushing audio (PipeWire `running` / PulseAudio not corked).
    pub running: bool,
    /// Output device `node.name`, when linked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sink: Option<String>,
}

impl AudioStream {
    /// Running, unmuted and above zero volume — what a listener would hear.
    pub fn audible(&self) -> bool {
        self.running && !self.muted && self.volume > 0.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sink {
    pub id: u32,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub backend: BackendKind,
    pub streams: Vec<AudioStream>,
    pub sinks: Vec<Sink>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuteMode {
    On,
    Off,
    Toggle,
}

impl MuteMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "mute" | "1" => Some(Self::On),
            "off" | "false" | "unmute" | "0" => Some(Self::Off),
            "toggle" => Some(Self::Toggle),
            _ => None,
        }
    }

    fn tool_arg(self) -> &'static str {
        match self {
            Self::On => "1",
            Self::Off => "0",
            Self::Toggle => "toggle",
        }
    }
}

// ---------------------------------------------------------------------------
// Backend over host tools
// ---------------------------------------------------------------------------

pub struct Streams {
    runner: Arc<dyn CommandRunner>,
}

impl Streams {
    pub fn real() -> Self {
        Self { runner: Arc::new(RealRunner) }
    }

    #[cfg(test)]
    pub fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let out = self
            .runner
            .run(program, args)
            .await
            .map_err(|e| format!("ERR_MEDIA_AUDIO_UNAVAILABLE: {e}"))?;
        if !out.ok {
            return Err(format!(
                "ERR_MEDIA_AUDIO_UNAVAILABLE: {program} exited nonzero: {}",
                out.stderr.trim()
            ));
        }
        Ok(out.stdout)
    }

    /// PipeWire first; PulseAudio only when `pw-dump` is not installed (a
    /// failing `pw-dump` with the binary present means PipeWire is broken,
    /// and `pactl` would talk to the same daemon anyway).
    pub async fn snapshot(&self) -> Result<Snapshot, String> {
        match self.runner.run("pw-dump", &[]).await {
            Ok(out) if out.ok => parse_pw_dump(&out.stdout),
            Ok(out) => Err(format!(
                "ERR_MEDIA_AUDIO_UNAVAILABLE: pw-dump exited nonzero: {}",
                out.stderr.trim()
            )),
            Err(RunnerError::NotFound(_)) => {
                let inputs = self.run("pactl", &["-f", "json", "list", "sink-inputs"]).await?;
                let sinks = self.run("pactl", &["-f", "json", "list", "sinks"]).await?;
                parse_pactl(&inputs, &sinks)
            }
            Err(e) => Err(format!("ERR_MEDIA_AUDIO_UNAVAILABLE: {e}")),
        }
    }

    pub async fn set_volume(&self, backend: BackendKind, id: u32, level: f64) -> Result<(), String> {
        let id = id.to_string();
        match backend {
            BackendKind::Pipewire => {
                let frac = format!("{level:.3}");
                self.run("wpctl", &["set-volume", &id, &frac]).await?;
            }
            BackendKind::Pulse => {
                let pct = format!("{}%", (level * 100.0).round() as u32);
                self.run("pactl", &["set-sink-input-volume", &id, &pct]).await?;
            }
        }
        Ok(())
    }

    pub async fn set_mute(&self, backend: BackendKind, id: u32, mode: MuteMode) -> Result<(), String> {
        let id = id.to_string();
        match backend {
            BackendKind::Pipewire => {
                self.run("wpctl", &["set-mute", &id, mode.tool_arg()]).await?;
            }
            BackendKind::Pulse => {
                self.run("pactl", &["set-sink-input-mute", &id, mode.tool_arg()]).await?;
            }
        }
        Ok(())
    }

    pub async fn move_to(&self, backend: BackendKind, id: u32, sink: &Sink) -> Result<(), String> {
        let id = id.to_string();
        match backend {
            BackendKind::Pipewire => {
                self.run("pw-metadata", &[&id, "target.object", &sink.name]).await?;
            }
            BackendKind::Pulse => {
                self.run("pactl", &["move-sink-input", &id, &sink.name]).await?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Parsers (pure)
// ---------------------------------------------------------------------------

fn as_u32(v: Option<&Value>) -> Option<u32> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .and_then(|n| u32::try_from(n).ok())
}

fn as_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "-")
        .map(str::to_string)
}

/// Parse `pw-dump` output: output streams, sinks, and the links between
/// them. Clients supply the PID and binary for their stream nodes.
pub fn parse_pw_dump(json: &str) -> Result<Snapshot, String> {
    let objects: Vec<Value> = serde_json::from_str(json)
        .map_err(|e| format!("ERR_MEDIA_AUDIO_UNAVAILABLE: unparseable pw-dump output: {e}"))?;

    let of_type = |t: &'static str| objects.iter().filter(move |o| o["type"] == t);

    let clients: std::collections::HashMap<u32, &Value> = of_type("PipeWire:Interface:Client")
        .filter_map(|c| Some((as_u32(c.get("id"))?, &c["info"]["props"])))
        .collect();

    let sinks: Vec<Sink> = of_type("PipeWire:Interface:Node")
        .filter(|n| n["info"]["props"]["media.class"] == SINK_CLASS)
        .filter_map(|n| {
            let props = &n["info"]["props"];
            Some(Sink {
                id: as_u32(n.get("id"))?,
                name: as_string(props.get("node.name"))?,
                description: as_string(props.get("node.description")),
            })
        })
        .collect();

    // Output node -> first linked input node (the sink it plays into).
    let mut links: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for l in of_type("PipeWire:Interface:Link") {
        if let (Some(o), Some(i)) = (
            as_u32(l["info"].get("output-node-id")),
            as_u32(l["info"].get("input-node-id")),
        ) {
            links.entry(o).or_insert(i);
        }
    }

    let streams = of_type("PipeWire:Interface:Node")
        .filter(|n| n["info"]["props"]["media.class"] == STREAM_CLASS)
        .filter_map(|n| {
            let id = as_u32(n.get("id"))?;
            let props = &n["info"]["props"];
            let client = as_u32(props.get("client.id")).and_then(|c| clients.get(&c).copied());
            let pick = |key: &str| {
                as_string(props.get(key)).or_else(|| client.and_then(|c| as_string(c.get(key))))
            };
            let pid = as_u32(props.get("application.process.id")).or_else(|| {
                client.and_then(|c| {
                    as_u32(c.get("pipewire.sec.pid")).or_else(|| as_u32(c.get("application.process.id")))
                })
            });
            let p = &n["info"]["params"]["Props"][0];
            let linear = p["channelVolumes"]
                .as_array()
                .and_then(|a| a.iter().filter_map(Value::as_f64).reduce(f64::max))
                .unwrap_or(1.0)
                * p["volume"].as_f64().unwrap_or(1.0);
            Some(AudioStream {
                id,
                app: pick("application.name")
                    .or_else(|| as_string(props.get("node.name")))
                    .unwrap_or_else(|| format!("stream {id}")),
                media_name: as_string(props.get("media.name")),
                pid,
                binary: pick("application.process.binary"),
                volume: round3(linear.max(0.0).cbrt()),
                muted: p["mute"].as_bool().unwrap_or(false),
                running: n["info"]["state"] == "running",
                sink: links
                    .get(&id)
                    .and_then(|sid| sinks.iter().find(|s| s.id == *sid))
                    .map(|s| s.name.clone()),
            })
        })
        .collect();

    Ok(Snapshot { backend: BackendKind::Pipewire, streams, sinks })
}

/// Parse `pactl -f json list sink-inputs` + `pactl -f json list sinks`.
pub fn parse_pactl(inputs_json: &str, sinks_json: &str) -> Result<Snapshot, String> {
    let bad = |e: serde_json::Error| format!("ERR_MEDIA_AUDIO_UNAVAILABLE: unparseable pactl output: {e}");
    let inputs: Vec<Value> = serde_json::from_str(inputs_json).map_err(bad)?;
    let raw_sinks: Vec<Value> = serde_json::from_str(sinks_json).map_err(bad)?;

    let sinks: Vec<Sink> = raw_sinks
        .iter()
        .filter_map(|s| {
            Some(Sink {
                id: as_u32(s.get("index"))?,
                name: as_string(s.get("name"))?,
                description: as_string(s.get("description")),
            })
        })
        .collect();

    let streams = inputs
        .iter()
        .filter_map(|i| {
            let id = as_u32(i.get("index"))?;
            let props = &i["properties"];
            // Highest channel percentage; pactl's percent is already cubic.
            let volume = i["volume"]
                .as_object()
                .map(|chans| {
                    chans
                        .values()
                        .filter_map(|c| c["value_percent"].as_str())
                        .filter_map(|p| p.trim_end_matches('%').trim().parse::<f64>().ok())
                        .fold(0.0, f64::max)
                })
                .unwrap_or(0.0)
                / 100.0;
            let sink_id = as_u32(i.get("sink"));
            Some(AudioStream {
                id,
                app: as_string(props.get("application.name")).unwrap_or_else(|| format!("stream {id}")),
                media_name: as_string(props.get("media.name")),
                pid: as_u32(props.get("application.process.id")),
                binary: as_string(props.get("application.process.binary")),
                volume: round3(volume),
                muted: i["mute"].as_bool().unwrap_or(false),
                running: !i["corked"].as_bool().unwrap_or(false),
                sink: sink_id.and_then(|sid| sinks.iter().find(|s| s.id == sid)).map(|s| s.name.clone()),
            })
        })
        .collect();

    Ok(Snapshot { backend: BackendKind::Pulse, streams, sinks })
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

// ---------------------------------------------------------------------------
// Matching streams to players / apps / sinks
// ---------------------------------------------------------------------------

/// Parent PID from `/proc/<pid>/stat` (field 4, after the parenthesised
/// comm, which may itself contain spaces or parens).
pub fn proc_parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// `org.mpris.MediaPlayer2.firefox.instance_1_424` -> `firefox`.
pub fn player_short_name(player: &str) -> String {
    let short = player.strip_prefix("org.mpris.MediaPlayer2.").unwrap_or(player);
    short.split('.').next().unwrap_or(short).to_ascii_lowercase()
}

/// Streams belonging to an MPRIS player: PID (or an ancestor of the stream
/// PID) equal to the player's D-Bus owner PID. Only when no stream matches
/// by PID does the name heuristic run (app name / binary contains the
/// player's short name), for backends that report no PID.
pub fn streams_for_player<'a>(
    streams: &'a [AudioStream],
    player: &str,
    player_pid: Option<u32>,
    parent_of: &dyn Fn(u32) -> Option<u32>,
) -> Vec<&'a AudioStream> {
    if let Some(ppid) = player_pid {
        let by_pid: Vec<&AudioStream> = streams
            .iter()
            .filter(|s| {
                let mut cur = s.pid;
                for _ in 0..=MAX_PID_HOPS {
                    match cur {
                        Some(p) if p == ppid => return true,
                        Some(p) if p > 1 => cur = parent_of(p),
                        _ => return false,
                    }
                }
                false
            })
            .collect();
        if !by_pid.is_empty() {
            return by_pid;
        }
    }
    let short = player_short_name(player);
    if short.is_empty() {
        return Vec::new();
    }
    streams
        .iter()
        .filter(|s| {
            s.app.to_ascii_lowercase().contains(&short)
                || s.binary.as_deref().is_some_and(|b| b.to_ascii_lowercase().contains(&short))
        })
        .collect()
}

/// Streams whose app name, binary or media name contains `query`
/// (case-insensitive).
pub fn streams_for_app<'a>(streams: &'a [AudioStream], query: &str) -> Vec<&'a AudioStream> {
    let q = query.trim().to_ascii_lowercase();
    streams
        .iter()
        .filter(|s| {
            s.app.to_ascii_lowercase().contains(&q)
                || s.binary.as_deref().is_some_and(|b| b.to_ascii_lowercase().contains(&q))
        })
        .collect()
}

/// Resolve a sink by exact id, exact `node.name`, or a unique
/// case-insensitive substring of name/description ("bluetooth" won't match,
/// but "jbl" or "bluez" will).
pub fn find_sink<'a>(sinks: &'a [Sink], query: &str) -> Result<&'a Sink, String> {
    let q = query.trim();
    if let Ok(id) = q.parse::<u32>() {
        if let Some(s) = sinks.iter().find(|s| s.id == id) {
            return Ok(s);
        }
    }
    if let Some(s) = sinks.iter().find(|s| s.name == q) {
        return Ok(s);
    }
    let ql = q.to_ascii_lowercase();
    let hits: Vec<&Sink> = sinks
        .iter()
        .filter(|s| {
            s.name.to_ascii_lowercase().contains(&ql)
                || s.description.as_deref().is_some_and(|d| d.to_ascii_lowercase().contains(&ql))
        })
        .collect();
    let names = || sinks.iter().map(|s| s.name.as_str()).collect::<Vec<_>>();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("ERR_MEDIA_SINK_NOT_FOUND: no output device matches '{q}' (have: {:?})", names())),
        many => Err(format!(
            "ERR_MEDIA_AMBIGUOUS: '{q}' matches several output devices: {:?}",
            many.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunOutcome;
    use std::sync::Mutex;

    const PW_DUMP: &str = include_str!("testdata/pw-dump.json");

    fn snap() -> Snapshot {
        parse_pw_dump(PW_DUMP).unwrap()
    }

    fn stream(s: &Snapshot, id: u32) -> &AudioStream {
        s.streams.iter().find(|x| x.id == id).unwrap()
    }

    #[test]
    fn pw_dump_lists_only_output_streams() {
        let s = snap();
        let ids: Vec<u32> = s.streams.iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![73, 78, 121], "input stream 130 must be skipped");
        assert_eq!(s.backend, BackendKind::Pipewire);
    }

    #[test]
    fn pw_dump_volume_is_cubic() {
        let s = snap();
        // channelVolumes 0.125006 linear == 0.5 on the wpctl/pactl scale.
        assert_eq!(stream(&s, 78).volume, 0.5);
        assert_eq!(stream(&s, 73).volume, 0.44);
    }

    #[test]
    fn pw_dump_joins_client_pid_and_binary() {
        let s = snap();
        let spotify = stream(&s, 78);
        assert_eq!(spotify.pid, Some(3_499_887));
        assert_eq!(spotify.binary.as_deref(), Some("spotify"));
        assert_eq!(spotify.app, "Spotify");
    }

    #[test]
    fn pw_dump_resolves_sink_via_links() {
        let s = snap();
        assert_eq!(stream(&s, 78).sink.as_deref(), Some("alsa_output.pci-0000_00_1b.0.analog-stereo"));
        assert_eq!(stream(&s, 121).sink.as_deref(), Some("bluez_output.41_42_31_87_87_59.1"));
        assert_eq!(s.sinks.len(), 2);
    }

    #[test]
    fn pw_dump_state_and_mute_drive_audible() {
        let s = snap();
        assert!(stream(&s, 78).audible());
        assert!(!stream(&s, 73).audible(), "idle mpd is not audible");
        assert!(!stream(&s, 121).audible(), "muted firefox is not audible");
    }

    #[test]
    fn pw_dump_media_name_dash_is_dropped() {
        let s = snap();
        assert_eq!(stream(&s, 73).media_name, None);
    }

    #[test]
    fn pw_dump_garbage_errors() {
        assert!(parse_pw_dump("not json").unwrap_err().contains("ERR_MEDIA_AUDIO_UNAVAILABLE"));
    }

    const PACTL_INPUTS: &str = r#"[
      {"index":8597,"sink":57,"mute":false,"corked":true,
       "volume":{"front-left":{"value":28836,"value_percent":"44%"},"front-right":{"value":28836,"value_percent":"44%"}},
       "properties":{"application.name":"Music Player Daemon","application.process.id":null,"media.name":" - "}},
      {"index":36188,"sink":59,"mute":true,"corked":false,
       "volume":{"front-left":{"value_percent":"50%"},"front-right":{"value_percent":"60%"}},
       "properties":{"application.name":"Spotify","application.process.id":"3499887","application.process.binary":"spotify"}}
    ]"#;
    const PACTL_SINKS: &str = r#"[
      {"index":57,"name":"alsa_output.pci-0000_00_1b.0.analog-stereo","description":"Built-in Audio Analog Stereo"},
      {"index":59,"name":"alsa_output.platform-snd_aloop.0.analog-stereo","description":"Loopback Analog Stereo"}
    ]"#;

    #[test]
    fn pactl_parses_streams_and_sinks() {
        let s = parse_pactl(PACTL_INPUTS, PACTL_SINKS).unwrap();
        assert_eq!(s.backend, BackendKind::Pulse);
        let mpd = stream(&s, 8597);
        assert_eq!(mpd.volume, 0.44);
        assert!(!mpd.running, "corked means paused");
        assert_eq!(mpd.pid, None);
        let spotify = stream(&s, 36188);
        assert_eq!(spotify.volume, 0.6, "loudest channel wins");
        assert!(spotify.muted);
        assert_eq!(spotify.pid, Some(3_499_887), "string pid accepted");
        assert_eq!(spotify.sink.as_deref(), Some("alsa_output.platform-snd_aloop.0.analog-stereo"));
    }

    #[test]
    fn player_matches_by_exact_pid() {
        let s = snap();
        let hits = streams_for_player(&s.streams, "org.mpris.MediaPlayer2.spotify", Some(3_499_887), &|_| None);
        assert_eq!(hits.iter().map(|x| x.id).collect::<Vec<_>>(), vec![78]);
    }

    #[test]
    fn player_matches_via_ancestor_pid() {
        // Firefox: stream from content process 5002, MPRIS owned by parent 4000.
        let s = snap();
        let parent = |p: u32| if p == 5002 { Some(4000) } else { None };
        let hits = streams_for_player(&s.streams, "org.mpris.MediaPlayer2.firefox.instance_1_424", Some(4000), &parent);
        assert_eq!(hits.iter().map(|x| x.id).collect::<Vec<_>>(), vec![121]);
    }

    #[test]
    fn player_falls_back_to_name_when_pid_unknown() {
        let s = parse_pactl(PACTL_INPUTS, PACTL_SINKS).unwrap();
        let hits = streams_for_player(&s.streams, "org.mpris.MediaPlayer2.spotify", None, &|_| None);
        assert_eq!(hits.iter().map(|x| x.id).collect::<Vec<_>>(), vec![36188]);
    }

    #[test]
    fn player_name_fallback_uses_binary() {
        // "mpd" is not in "Music Player Daemon" but is in binary "mpd (deleted)".
        let s = snap();
        let hits = streams_for_player(&s.streams, "org.mpris.MediaPlayer2.mpd", Some(999), &|_| None);
        assert_eq!(hits.iter().map(|x| x.id).collect::<Vec<_>>(), vec![73]);
    }

    #[test]
    fn short_name_strips_prefix_and_instance() {
        assert_eq!(player_short_name("org.mpris.MediaPlayer2.firefox.instance_1_424"), "firefox");
        assert_eq!(player_short_name("org.mpris.MediaPlayer2.spotify"), "spotify");
    }

    #[test]
    fn app_query_is_case_insensitive() {
        let s = snap();
        let hits = streams_for_app(&s.streams, "FIREfox");
        assert_eq!(hits.iter().map(|x| x.id).collect::<Vec<_>>(), vec![121]);
    }

    #[test]
    fn find_sink_by_id_name_and_substring() {
        let s = snap();
        assert_eq!(find_sink(&s.sinks, "34").unwrap().id, 34);
        assert_eq!(find_sink(&s.sinks, "alsa_output.pci-0000_00_1b.0.analog-stereo").unwrap().id, 57);
        assert_eq!(find_sink(&s.sinks, "jbl").unwrap().id, 34);
        assert!(find_sink(&s.sinks, "hdmi").unwrap_err().starts_with("ERR_MEDIA_SINK_NOT_FOUND"));
        assert!(find_sink(&s.sinks, "o").unwrap_err().starts_with("ERR_MEDIA_AMBIGUOUS"));
    }

    #[test]
    fn mute_mode_parse() {
        assert_eq!(MuteMode::parse("ON"), Some(MuteMode::On));
        assert_eq!(MuteMode::parse("unmute"), Some(MuteMode::Off));
        assert_eq!(MuteMode::parse("toggle"), Some(MuteMode::Toggle));
        assert_eq!(MuteMode::parse("maybe"), None);
    }

    #[test]
    fn proc_parent_reads_own_process() {
        let me = std::process::id();
        assert!(proc_parent(me).is_some());
    }

    // ---- backend argv via a recording fake runner ----

    struct FakeRunner {
        missing: Vec<&'static str>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(&self, program: &str, args: &[&str]) -> Result<RunOutcome, RunnerError> {
            self.calls.lock().unwrap().push(format!("{program} {}", args.join(" ")));
            if self.missing.contains(&program) {
                return Err(RunnerError::NotFound(program.into()));
            }
            let stdout = match (program, args) {
                ("pw-dump", _) => PW_DUMP.to_string(),
                ("pactl", [_, _, _, "sink-inputs"]) => PACTL_INPUTS.to_string(),
                ("pactl", [_, _, _, "sinks"]) => PACTL_SINKS.to_string(),
                _ => String::new(),
            };
            Ok(RunOutcome { ok: true, stdout, stderr: String::new() })
        }
    }

    fn fake(missing: Vec<&'static str>) -> Arc<FakeRunner> {
        Arc::new(FakeRunner { missing, calls: Mutex::new(Vec::new()) })
    }

    #[tokio::test]
    async fn snapshot_prefers_pipewire() {
        let r = fake(vec![]);
        let s = Streams::with_runner(r.clone()).snapshot().await.unwrap();
        assert_eq!(s.backend, BackendKind::Pipewire);
        assert_eq!(r.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn snapshot_falls_back_to_pactl_when_pw_dump_missing() {
        let s = Streams::with_runner(fake(vec!["pw-dump"])).snapshot().await.unwrap();
        assert_eq!(s.backend, BackendKind::Pulse);
    }

    #[tokio::test]
    async fn snapshot_errors_when_no_audio_tools() {
        let err = Streams::with_runner(fake(vec!["pw-dump", "pactl"])).snapshot().await.unwrap_err();
        assert!(err.starts_with("ERR_MEDIA_AUDIO_UNAVAILABLE"), "{err}");
    }

    #[tokio::test]
    async fn pipewire_commands_argv() {
        let r = fake(vec![]);
        let st = Streams::with_runner(r.clone());
        let sink = Sink { id: 34, name: "bluez_output.x".into(), description: None };
        st.set_volume(BackendKind::Pipewire, 78, 0.3).await.unwrap();
        st.set_mute(BackendKind::Pipewire, 78, MuteMode::Toggle).await.unwrap();
        st.move_to(BackendKind::Pipewire, 78, &sink).await.unwrap();
        assert_eq!(
            *r.calls.lock().unwrap(),
            vec![
                "wpctl set-volume 78 0.300",
                "wpctl set-mute 78 toggle",
                "pw-metadata 78 target.object bluez_output.x",
            ]
        );
    }

    #[tokio::test]
    async fn pulse_commands_argv() {
        let r = fake(vec![]);
        let st = Streams::with_runner(r.clone());
        let sink = Sink { id: 59, name: "alsa_output.x".into(), description: None };
        st.set_volume(BackendKind::Pulse, 8597, 0.25).await.unwrap();
        st.set_mute(BackendKind::Pulse, 8597, MuteMode::On).await.unwrap();
        st.move_to(BackendKind::Pulse, 8597, &sink).await.unwrap();
        assert_eq!(
            *r.calls.lock().unwrap(),
            vec![
                "pactl set-sink-input-volume 8597 25%",
                "pactl set-sink-input-mute 8597 1",
                "pactl move-sink-input 8597 alsa_output.x",
            ]
        );
    }
}
