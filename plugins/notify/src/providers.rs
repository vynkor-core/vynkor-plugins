//! Delivery backends: `notify-send` (libnotify desktop notifications),
//! `wall` (broadcast to all logged-in terminals), and `espeak` (spoken
//! alert via `espeak-ng`, falling back to `espeak`).
//!
//! Every delivery spawns the binary directly with argv — never a shell —
//! so message/title content cannot inject commands. Operator policy lives
//! in the `NOTIFY_PLUGIN_ENABLED_PROVIDERS` env var (comma-separated;
//! empty = all enabled).

use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use vynkor_sdk::proto::ActionStatus;
use vynkor_sdk::VynkorClient;

use crate::request::NotifyParams;

/// Operator env var: comma-separated enabled providers (`notify-send`,
/// `wall`, `espeak`). Empty = all enabled. Unknown ids are an error.
pub const ENABLED_PROVIDERS_ENV: &str = "NOTIFY_PLUGIN_ENABLED_PROVIDERS";
/// Operator env var: default notify-send app name.
pub const APP_NAME_ENV: &str = "NOTIFY_PLUGIN_APP_NAME";
/// Fallback notify-send app name when neither the request nor the env var
/// sets one.
pub const DEFAULT_APP_NAME: &str = "vynkor";

/// Operator env var: tts provider for `speak: true` озвучка.
pub const TTS_PROVIDER_ENV: &str = "NOTIFY_PLUGIN_TTS_PROVIDER";
/// Operator env var: tts voice id (optional; the `voice` key is omitted when
/// unset/empty).
pub const TTS_VOICE_ENV: &str = "NOTIFY_PLUGIN_TTS_VOICE";
/// Operator env var: tts output container format.
pub const TTS_FORMAT_ENV: &str = "NOTIFY_PLUGIN_TTS_FORMAT";
/// Operator env var: audio player binary override for `speak: true`.
pub const AUDIO_PLAYER_ENV: &str = "NOTIFY_PLUGIN_AUDIO_PLAYER";

pub const DEFAULT_TTS_PROVIDER: &str = "sherpa";
pub const DEFAULT_TTS_FORMAT: &str = "wav";
/// Timeout for the outbound `tts_synthesize` call (matches tts's own cap).
pub const TTS_TIMEOUT_MS: u32 = 60_000;

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The three supported delivery providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// libnotify desktop notification (`notify-send`). Primary provider.
    NotifySend,
    /// Broadcast to all logged-in terminals (`wall`, util-linux).
    Wall,
    /// Spoken alert (`espeak-ng`, falling back to `espeak`).
    Espeak,
}

impl ProviderKind {
    /// Parse a provider id; unknown ids get an error naming the valid set.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "notify-send" => Ok(Self::NotifySend),
            "wall" => Ok(Self::Wall),
            "espeak" => Ok(Self::Espeak),
            other => Err(format!(
                "unknown provider '{other}': valid providers are notify-send, wall, espeak"
            )),
        }
    }

    /// Canonical provider id.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotifySend => "notify-send",
            Self::Wall => "wall",
            Self::Espeak => "espeak",
        }
    }

    /// One-line description of what the provider does.
    pub fn description(&self) -> &'static str {
        match self {
            Self::NotifySend => "libnotify desktop notification via notify-send",
            Self::Wall => "broadcast a message to all logged-in terminals (wall)",
            Self::Espeak => "spoken alert via espeak-ng (falls back to espeak)",
        }
    }
}

fn all_providers() -> Vec<ProviderKind> {
    vec![
        ProviderKind::NotifySend,
        ProviderKind::Wall,
        ProviderKind::Espeak,
    ]
}

/// True when `name` is a file on `PATH` and executable (on unix: any
/// execute bit set — checked via `PermissionsExt::mode() & 0o111`).
pub fn binary_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file() && is_executable(&candidate)
            })
        })
        .unwrap_or(false)
}

/// Executable check for a candidate file. On unix this is the classic
/// execute-bit test; elsewhere any file counts (the provider binaries are
/// Linux-only anyway — see README).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

/// True when the provider's binary is installed.
fn kind_available(kind: ProviderKind) -> bool {
    match kind {
        ProviderKind::NotifySend => binary_in_path("notify-send"),
        ProviderKind::Wall => binary_in_path("wall"),
        ProviderKind::Espeak => binary_in_path("espeak-ng") || binary_in_path("espeak"),
    }
}

/// One provider entry for the `notify_providers` response.
#[derive(Debug, Serialize)]
pub struct ProviderInfo {
    /// Canonical provider id (`notify-send`, `wall`, `espeak`).
    pub id: String,
    /// Display name (same as the id for all three providers).
    pub name: String,
    /// True when the provider is enabled by operator policy AND its binary
    /// is installed.
    pub available: bool,
    pub description: String,
}

/// All three providers with their availability (enabled by the operator's
/// `NOTIFY_PLUGIN_ENABLED_PROVIDERS` AND binary present on `PATH`).
///
/// Env-parse errors fall back to "all enabled" here on purpose: the
/// reporting path stays readable while [`deliver`] — the gate that
/// matters — still surfaces the error on every call.
pub fn list_providers() -> Vec<ProviderInfo> {
    let enabled = enabled_providers().unwrap_or_else(|_| all_providers());
    list_providers_for(&enabled)
}

/// Pure version of [`list_providers`] with the enabled set injected — keeps
/// tests independent of the ambient environment.
fn list_providers_for(enabled: &[ProviderKind]) -> Vec<ProviderInfo> {
    all_providers()
        .into_iter()
        .map(|kind| ProviderInfo {
            id: kind.as_str().to_string(),
            name: kind.as_str().to_string(),
            available: enabled.contains(&kind) && kind_available(kind),
            description: kind.description().to_string(),
        })
        .collect()
}

/// Parse the operator's `NOTIFY_PLUGIN_ENABLED_PROVIDERS` list. Empty or
/// all-whitespace = every provider enabled. Unknown ids are an error.
pub fn parse_enabled_list(raw: &str) -> Result<Vec<ProviderKind>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(all_providers());
    }
    let mut kinds = Vec::new();
    for part in trimmed.split(',') {
        let id = part.trim();
        if id.is_empty() {
            continue;
        }
        let kind = ProviderKind::parse(id)?;
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    Ok(kinds)
}

/// The enabled provider set from the env var (unset or empty = all three).
pub fn enabled_providers() -> Result<Vec<ProviderKind>, String> {
    match std::env::var(ENABLED_PROVIDERS_ENV) {
        Ok(raw) => parse_enabled_list(&raw),
        Err(std::env::VarError::NotPresent) => Ok(all_providers()),
        Err(e) => Err(format!("failed to read {ENABLED_PROVIDERS_ENV}: {e}")),
    }
}

fn enabled_summary(enabled: &[ProviderKind]) -> String {
    enabled
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// What got delivered and how, reported back to the caller.
#[derive(Debug, Serialize)]
pub struct Delivered {
    /// Canonical provider id the message went through.
    pub provider: String,
    /// The binary that was invoked.
    pub command: String,
    /// Trimmed stdout of the delivery binary (empty when it prints nothing).
    pub detail: String,
}

/// Deliver `params` through `kind`. Checks operator policy (enabled
/// providers) first, then binary availability.
pub async fn deliver(kind: ProviderKind, params: &NotifyParams) -> Result<Delivered, String> {
    let enabled = enabled_providers()?;
    deliver_with(kind, params, &enabled).await
}

/// Pure version of [`deliver`] with the enabled set injected — keeps tests
/// independent of the ambient environment.
async fn deliver_with(
    kind: ProviderKind,
    params: &NotifyParams,
    enabled: &[ProviderKind],
) -> Result<Delivered, String> {
    if !enabled.contains(&kind) {
        return Err(format!(
            "provider '{}' is disabled by {ENABLED_PROVIDERS_ENV} (enabled: {})",
            kind.as_str(),
            enabled_summary(enabled)
        ));
    }
    match kind {
        ProviderKind::NotifySend => deliver_notify_send(params).await,
        ProviderKind::Wall => deliver_wall(params).await,
        ProviderKind::Espeak => deliver_espeak(params).await,
    }
}

/// Build the argv for `notify-send`, in order:
/// `-a <app>` `-u <urgency>` `-t <ms>` `[title]` `message`.
///
/// Pure with respect to the request; the app-name env var is read here, so
/// argument *shape* stays deterministic under test via
/// [`notify_send_args_with`].
pub fn notify_send_args(params: &NotifyParams) -> Vec<String> {
    let env_app_name = std::env::var(APP_NAME_ENV).ok();
    notify_send_args_with(params, env_app_name.as_deref())
}

/// [`notify_send_args`] with the operator's app-name env var injected.
fn notify_send_args_with(params: &NotifyParams, env_app_name: Option<&str>) -> Vec<String> {
    let app_name = params
        .app_name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            env_app_name
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_APP_NAME.to_string());
    let mut args = vec!["-a".to_string(), app_name];
    if let Some(urgency) = &params.urgency {
        args.push("-u".to_string());
        args.push(urgency.clone());
    }
    if let Some(timeout) = params.timeout_ms {
        if timeout > 0 {
            args.push("-t".to_string());
            args.push(timeout.to_string());
        }
    }
    if !params.title.is_empty() {
        args.push(params.title.clone());
    }
    args.push(params.message.clone());
    args
}

/// The single text argument `wall` and `espeak` get: `"title: message"` when
/// a title is set, else just the message.
pub fn full_text(params: &NotifyParams) -> String {
    if params.title.is_empty() {
        params.message.clone()
    } else {
        format!("{}: {}", params.title, params.message)
    }
}

async fn deliver_notify_send(params: &NotifyParams) -> Result<Delivered, String> {
    if !binary_in_path("notify-send") {
        return Err(missing_binary("notify-send"));
    }
    let args = notify_send_args(params);
    let detail = run_command("notify-send", &args).await?;
    Ok(Delivered {
        provider: "notify-send".to_string(),
        command: "notify-send".to_string(),
        detail,
    })
}

async fn deliver_wall(params: &NotifyParams) -> Result<Delivered, String> {
    if !binary_in_path("wall") {
        return Err(missing_binary("wall"));
    }
    let detail = run_command("wall", &[full_text(params)]).await?;
    Ok(Delivered {
        provider: "wall".to_string(),
        command: "wall".to_string(),
        detail,
    })
}

async fn deliver_espeak(params: &NotifyParams) -> Result<Delivered, String> {
    let bin = if binary_in_path("espeak-ng") {
        "espeak-ng"
    } else if binary_in_path("espeak") {
        "espeak"
    } else {
        return Err(
            "espeak provider requires 'espeak-ng' (or 'espeak'): neither was found on PATH (install espeak-ng and retry)"
                .to_string(),
        );
    };
    let detail = run_command(bin, &[full_text(params)]).await?;
    Ok(Delivered {
        provider: "espeak".to_string(),
        command: bin.to_string(),
        detail,
    })
}

fn missing_binary(bin: &str) -> String {
    format!(
        "provider requires the '{bin}' binary, which was not found on PATH (install it and retry)"
    )
}

/// Spawn `bin` with `args` (argv only — never a shell), capture output, and
/// map the result: spawn failure → "failed to spawn {bin}: {e}"; non-zero
/// exit → trimmed stderr (falling back to "see system logs" when empty);
/// success → trimmed stdout as the detail string.
async fn run_command(bin: &str, args: &[String]) -> Result<String, String> {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to spawn {bin}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        let detail = if trimmed.is_empty() {
            "see system logs"
        } else {
            trimmed
        };
        return Err(format!("{bin} exited with {}: {detail}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Build the `tts_synthesize` params JSON for the `tts` plugin. Pure: the
/// operator's provider/voice/format are injected as arguments so tests stay
/// independent of the ambient environment. The `voice` key is omitted when
/// `None` (or empty).
pub fn build_tts_request(
    text: &str,
    provider: &str,
    voice: Option<&str>,
    format: &str,
) -> serde_json::Value {
    let mut request = serde_json::json!({
        "provider": provider,
        "text": text,
        "format": format,
        "timeout_ms": TTS_TIMEOUT_MS,
    });
    if let Some(v) = voice {
        if !v.trim().is_empty() {
            request["voice"] = serde_json::Value::String(v.to_string());
        }
    }
    request
}

/// Resolve the audio player for `format`: the operator's
/// [`AUDIO_PLAYER_ENV`] wins; otherwise `wav` auto-detects
/// `paplay` / `pw-play` / `aplay`, and any other format needs `ffplay`.
fn pick_player(format: &str) -> Result<String, String> {
    if let Ok(player) = std::env::var(AUDIO_PLAYER_ENV) {
        let player = player.trim().to_string();
        if player.is_empty() {
            return Err(format!("{AUDIO_PLAYER_ENV} is set but empty"));
        }
        return Ok(player);
    }
    if format == "wav" {
        for candidate in ["paplay", "pw-play", "aplay"] {
            if binary_in_path(candidate) {
                return Ok(candidate.to_string());
            }
        }
        return Err(
            "no audio player found for wav: install paplay (libpulse), pw-play (pipewire), \
             or aplay (alsa-utils), or set NOTIFY_PLUGIN_AUDIO_PLAYER"
                .to_string(),
        );
    }
    if binary_in_path("ffplay") {
        return Ok("ffplay".to_string());
    }
    Err(format!(
        "no audio player found for '{format}': install ffmpeg (provides ffplay) or set {AUDIO_PLAYER_ENV}"
    ))
}

/// Speak `text` through the `tts` plugin: synthesize via `tts_synthesize`,
/// base64-decode the returned audio, write it to a temp file, play it with
/// the resolved player, and remove the temp file (best-effort cleanup runs
/// on every path, including player failure).
pub async fn speak_via_tts(client: &mut VynkorClient, text: &str) -> Result<(), String> {
    let provider =
        std::env::var(TTS_PROVIDER_ENV).unwrap_or_else(|_| DEFAULT_TTS_PROVIDER.to_string());
    let voice = std::env::var(TTS_VOICE_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty());
    let format = std::env::var(TTS_FORMAT_ENV).unwrap_or_else(|_| DEFAULT_TTS_FORMAT.to_string());

    let request = build_tts_request(text, &provider, voice.as_deref(), &format);
    let params =
        serde_json::to_vec(&request).map_err(|e| format!("failed to encode tts request: {e}"))?;

    let resp = client
        .send_action("tts_synthesize", &params, TTS_TIMEOUT_MS)
        .await
        .map_err(|e| format!("tts plugin call failed: {e}"))?;
    if resp.status != ActionStatus::ActionOk as i32 {
        return Err(if resp.error.is_empty() {
            "tts plugin returned an error".to_string()
        } else {
            resp.error.clone()
        });
    }

    #[derive(Deserialize)]
    struct TtsResponse {
        format: String,
        audio_base64: String,
    }
    let tts: TtsResponse = serde_json::from_slice(&resp.data_json)
        .map_err(|e| format!("failed to decode tts response: {e}"))?;
    if tts.format == "pcm" {
        return Err("pcm format is not directly playable; use wav".to_string());
    }
    if tts.format.is_empty() || !tts.format.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("invalid tts audio format '{}'", tts.format));
    }
    let audio = base64::engine::general_purpose::STANDARD
        .decode(tts.audio_base64.as_bytes())
        .map_err(|e| format!("failed to decode tts audio: {e}"))?;

    let player = pick_player(&tts.format)?;
    let file = std::env::temp_dir().join(format!(
        "notify-tts-{}-{}.{}",
        std::process::id(),
        unix_millis(),
        tts.format
    ));
    std::fs::write(&file, &audio)
        .map_err(|e| format!("failed to write audio temp file {}: {e}", file.display()))?;

    let result = run_command(&player, &[file.to_string_lossy().to_string()]).await;
    let _ = std::fs::remove_file(&file);
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(message: &str) -> NotifyParams {
        NotifyParams {
            provider: "notify-send".to_string(),
            title: String::new(),
            message: message.to_string(),
            urgency: None,
            timeout_ms: None,
            app_name: None,
            silent: false,
            speak: false,
        }
    }

    #[test]
    fn parse_accepts_all_three_providers() {
        assert_eq!(
            ProviderKind::parse("notify-send").unwrap(),
            ProviderKind::NotifySend
        );
        assert_eq!(ProviderKind::parse("wall").unwrap(), ProviderKind::Wall);
        assert_eq!(ProviderKind::parse("espeak").unwrap(), ProviderKind::Espeak);
        // Ids are trimmed, so surrounding whitespace is tolerated.
        assert_eq!(ProviderKind::parse("  wall ").unwrap(), ProviderKind::Wall);
    }

    #[test]
    fn parse_rejects_unknown_provider_naming_valid_set() {
        let err = ProviderKind::parse("email").unwrap_err();
        assert!(err.contains("unknown provider 'email'"), "error was: {err}");
        assert!(
            err.contains("notify-send, wall, espeak"),
            "error was: {err}"
        );
    }

    #[test]
    fn notify_send_args_minimal_request() {
        let args = notify_send_args_with(&n("hello"), None);
        assert_eq!(args, vec!["-a", "vynkor", "hello"]);
    }

    #[test]
    fn notify_send_args_with_urgency_and_timeout() {
        let mut p = n("hello");
        p.urgency = Some("critical".to_string());
        p.timeout_ms = Some(5000);
        let args = notify_send_args_with(&p, None);
        assert_eq!(
            args,
            vec!["-a", "vynkor", "-u", "critical", "-t", "5000", "hello"]
        );
    }

    #[test]
    fn notify_send_args_includes_title_when_non_empty() {
        let mut p = n("hello");
        p.title = "Build done".to_string();
        let args = notify_send_args_with(&p, None);
        assert_eq!(args, vec!["-a", "vynkor", "Build done", "hello"]);
    }

    #[test]
    fn notify_send_args_omits_empty_title() {
        let p = n("hello"); // title defaults to ""
        let args = notify_send_args_with(&p, None);
        assert_eq!(args, vec!["-a", "vynkor", "hello"]);
    }

    #[test]
    fn notify_send_args_timeout_zero_is_omitted() {
        let mut p = n("hello");
        p.timeout_ms = Some(0);
        let args = notify_send_args_with(&p, None);
        assert_eq!(args, vec!["-a", "vynkor", "hello"]);
    }

    #[test]
    fn notify_send_args_uses_request_app_name_over_default() {
        let mut p = n("hello");
        p.app_name = Some("myapp".to_string());
        let args = notify_send_args_with(&p, None);
        assert_eq!(args, vec!["-a", "myapp", "hello"]);
    }

    #[test]
    fn notify_send_args_env_app_name_beats_default() {
        let args = notify_send_args_with(&n("hello"), Some("envapp"));
        assert_eq!(args, vec!["-a", "envapp", "hello"]);
    }

    #[test]
    fn notify_send_args_empty_request_app_name_falls_back() {
        let mut p = n("hello");
        p.app_name = Some(String::new());
        let args = notify_send_args_with(&p, Some("envapp"));
        assert_eq!(args, vec!["-a", "envapp", "hello"]);
    }

    #[test]
    fn full_text_prepends_title_when_set() {
        let mut p = n("hello");
        p.title = "Build done".to_string();
        assert_eq!(full_text(&p), "Build done: hello");
        assert_eq!(full_text(&n("hello")), "hello");
    }

    #[test]
    fn binary_in_path_finds_sh_and_misses_nonsense() {
        assert!(binary_in_path("sh"), "sh should be on PATH");
        assert!(!binary_in_path("notify-definitely-not-a-real-binary-xyzzy"));
    }

    #[test]
    fn list_providers_reports_all_three_with_bool_availability() {
        let infos = list_providers();
        assert_eq!(infos.len(), 3);
        assert_eq!(infos[0].id, "notify-send");
        assert_eq!(infos[1].id, "wall");
        assert_eq!(infos[2].id, "espeak");
        for info in &infos {
            assert!(!info.name.is_empty());
            assert!(!info.description.is_empty());
            let _: bool = info.available; // typed bool by construction
        }
    }

    #[test]
    fn list_providers_marks_everything_unavailable_when_nothing_enabled() {
        let infos = list_providers_for(&[]);
        assert_eq!(infos.len(), 3);
        assert!(infos.iter().all(|i| !i.available));
    }

    #[test]
    fn list_providers_availability_matches_binary_presence_when_all_enabled() {
        let infos = list_providers_for(&all_providers());
        let expected = [
            kind_available(ProviderKind::NotifySend),
            kind_available(ProviderKind::Wall),
            kind_available(ProviderKind::Espeak),
        ];
        for (info, avail) in infos.iter().zip(expected) {
            assert_eq!(
                info.available, avail,
                "availability mismatch for {}",
                info.id
            );
        }
    }

    #[test]
    fn parse_enabled_list_empty_means_all() {
        assert_eq!(parse_enabled_list("").unwrap(), all_providers());
        assert_eq!(parse_enabled_list("   ").unwrap(), all_providers());
    }

    #[test]
    fn parse_enabled_list_respects_list_with_trimming() {
        let kinds = parse_enabled_list(" wall , espeak ").unwrap();
        assert_eq!(kinds, vec![ProviderKind::Wall, ProviderKind::Espeak]);
    }

    #[test]
    fn parse_enabled_list_skips_empty_entries_and_deduplicates() {
        let kinds = parse_enabled_list("wall,, wall,espeak").unwrap();
        assert_eq!(kinds, vec![ProviderKind::Wall, ProviderKind::Espeak]);
    }

    #[test]
    fn parse_enabled_list_rejects_unknown_id() {
        let err = parse_enabled_list("wall,email").unwrap_err();
        assert!(err.contains("unknown provider 'email'"), "error was: {err}");
    }

    #[test]
    fn tts_request_omits_voice_when_none() {
        let req = build_tts_request("hello", "sherpa", None, "wav");
        assert!(
            req.get("voice").is_none(),
            "voice key must be absent: {req}"
        );
        assert_eq!(req["provider"], "sherpa");
        assert_eq!(req["text"], "hello");
        assert_eq!(req["format"], "wav");
        assert_eq!(req["timeout_ms"], TTS_TIMEOUT_MS);
    }

    #[test]
    fn tts_request_omits_empty_voice() {
        let req = build_tts_request("hello", "sherpa", Some(""), "wav");
        assert!(req.get("voice").is_none(), "empty voice = omitted: {req}");
        let req = build_tts_request("hello", "sherpa", Some("  "), "wav");
        assert!(req.get("voice").is_none());
    }

    #[test]
    fn tts_request_includes_voice_when_set() {
        let req = build_tts_request("hello", "sherpa", Some("af_heart"), "wav");
        assert_eq!(req["voice"], "af_heart");
    }

    #[test]
    fn tts_request_carries_provider_and_format() {
        let req = build_tts_request("hello", "openai", Some("alloy"), "mp3");
        assert_eq!(req["provider"], "openai");
        assert_eq!(req["format"], "mp3");
        assert_eq!(req["voice"], "alloy");
    }
}
