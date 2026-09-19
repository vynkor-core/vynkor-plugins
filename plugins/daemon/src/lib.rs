//! `daemon` plugin library crate: the headless always-on voice client for
//! the `agent` plugin — mic → stt → agent → tts → sound, with no business
//! logic of its own (root `ROADMAP.md`: "thin clients to `agent`").
//!
//! Every stage is an ordinary kernel-routed action call into a shipped
//! plugin (`mic`, `stt`, `agent`, `tts`, `sound`); this plugin owns only the
//! orchestration: the listen→think→speak cycle, the background turn loop and
//! its on/off state. The daemon itself declares just `PERMISSION_AUDIO`
//! (caller of the gated `mic_start`/`sound_play`) and
//! `PERMISSION_EVENT_PUBLISH` (turn events); everything it calls that is
//! ungated (`stt_listen_*`, `goal_start`, `tts_synthesize`) needs nothing.
//!
//! Outbound calls go through [`Rpc`], a channel-fronted proxy: handler and
//! timer tasks never touch the `VynkorClient` directly, because
//! `send_action` discards every non-matching inbound frame while it waits —
//! a turn started by the timer would silently eat user requests arriving
//! mid-turn. With the proxy the serve loop stays the single reader (same
//! rationale as calendar/sync-client; see `docs/PLUGIN_AUTHORING.md` §1).

pub mod request;

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{envelope, Envelope, EventPublish};

use request::{parse_request, DaemonRequest};

/// Slug of the plugin that receives mic's PCM stream and turns it into text.
/// Fixed for v0.1: stt is the only shipped transcript provider.
pub const STT_TARGET: &str = "stt";

/// Kernel-namespaced event `stt` publishes when the (opt-in) energy VAD
/// hears speech begin on a listen stream.
pub const EV_SPEECH_STARTED: &str = "plugin.stt.stt_speech_started";

/// Kernel-namespaced event `stt` publishes when an utterance ends —
/// `silence_ms` of quiet after real speech. This is the vad-mode endpoint.
pub const EV_SPEECH_ENDED: &str = "plugin.stt.stt_speech_ended";

/// Kernel-namespaced events the `hotkey` plugin publishes for push-to-talk.
pub const EV_HOTKEY_PRESSED: &str = "plugin.hotkey.hotkey_pressed";
pub const EV_HOTKEY_RELEASED: &str = "plugin.hotkey.hotkey_released";

/// How the daemon decides when the user stopped talking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListenMode {
    /// Fixed capture window (`turn_ms`) per turn — v0.1 behavior.
    #[default]
    Window,
    /// Open-ended capture; the turn ends when `stt` reports the utterance
    /// ended (`EV_SPEECH_ENDED`, requires `STT_PLUGIN_VAD=on` on the stt
    /// side) or a configured cap elapses. Enables hands-free conversation:
    /// enable once, talk whenever.
    Vad,
    /// Push-to-talk: idle until `hotkey_pressed`, capture while held, end
    /// on `hotkey_released`. Requires the `hotkey` plugin registered.
    Ptt,
}

impl ListenMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Vad => "vad",
            Self::Ptt => "ptt",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "window" => Some(Self::Window),
            "vad" => Some(Self::Vad),
            "ptt" => Some(Self::Ptt),
            _ => None,
        }
    }
}

/// In-process event bus: the serve loop forwards every inbound kernel
/// `Event` here; the vad/ptt listen stages subscribe and await their
/// endpoints. Broadcast because several waiters may coexist (a manual turn
/// while the ptt task idles).
#[derive(Clone)]
pub struct Bus {
    tx: tokio::sync::broadcast::Sender<(String, Value)>,
}

impl Bus {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(128);
        Self { tx }
    }

    /// Fan out one kernel event to all subscribers. Never fails: with no
    /// subscribers there is simply nobody to notify.
    pub fn send(&self, event_type: &str, payload: Value) {
        let _ = self.tx.send((event_type.to_string(), payload));
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<(String, Value)> {
        self.tx.subscribe()
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

/// Runtime configuration (environment-driven; see `config.example.yaml`).
#[derive(Debug, Clone)]
pub struct Config {
    /// Start with the background loop enabled.
    pub enabled_at_boot: bool,
    /// How turns decide when speech is over.
    pub mode: ListenMode,
    /// Mic capture window per voice turn (`window` mode only).
    pub turn_ms: u64,
    /// Delay between one turn's end and the next tick while enabled.
    pub gap_ms: u64,
    /// `vad` mode: max silence waited BEFORE any speech before giving up.
    pub vad_wait_ms: u64,
    /// `vad` mode: hard cap on one utterance even without an ended event.
    pub vad_max_utterance_ms: u64,
    /// `ptt` mode: hotkey binding id that triggers a turn.
    pub ptt_binding: String,
    /// `ptt` mode: auto-release when the key was held this long (ms) — a
    /// stuck key must not hold the mic forever.
    pub ptt_max_hold_ms: u64,
    /// Capture rate negotiated with `stt_listen_start` and `mic_start`.
    pub sample_rate_hz: u32,
    /// mic chunk duration.
    pub chunk_ms: u32,
    /// AudioStreamChunk stream_id shared by both sides of the mic→stt hop.
    pub stream_id: i32,
    /// `tts_synthesize` provider.
    pub tts_provider: String,
    /// Provider-specific voice id.
    pub tts_voice: String,
    /// Synthesis format handed to `sound_play`.
    pub tts_format: String,
    /// `goal_start` max_steps budget per turn.
    pub max_steps: u32,
    /// Per-call timeout for mic/stt/tts/sound round-trips.
    pub timeout_ms: u32,
    /// Timeout for `goal_start` — LLM loops run long.
    pub goal_timeout_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled_at_boot: false,
            mode: ListenMode::Window,
            turn_ms: 6_000,
            gap_ms: 2_000,
            vad_wait_ms: 30_000,
            vad_max_utterance_ms: 20_000,
            ptt_binding: "ptt".into(),
            ptt_max_hold_ms: 60_000,
            sample_rate_hz: 16_000,
            chunk_ms: 100,
            stream_id: 7,
            tts_provider: "sherpa".into(),
            tts_voice: "af_heart".into(),
            tts_format: "wav".into(),
            max_steps: 6,
            timeout_ms: 30_000,
            goal_timeout_ms: 120_000,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        let read_u64 = |k: &str| -> Option<u64> {
            std::env::var(k).ok().and_then(|s| s.trim().parse::<u64>().ok())
        };
        if let Ok(v) = std::env::var("DAEMON_PLUGIN_ENABLED") {
            let v = v.trim().to_ascii_lowercase();
            c.enabled_at_boot = !v.is_empty() && v != "false" && v != "0";
        }
        if let Ok(v) = std::env::var("DAEMON_PLUGIN_MODE") {
            if let Some(m) = ListenMode::parse(&v) {
                c.mode = m;
            }
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_TURN_MS") {
            c.turn_ms = v.clamp(100, 120_000);
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_GAP_MS") {
            c.gap_ms = v.clamp(50, 3_600_000);
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_VAD_WAIT_MS") {
            c.vad_wait_ms = v.clamp(1_000, 600_000);
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_VAD_MAX_UTTERANCE_MS") {
            c.vad_max_utterance_ms = v.clamp(500, 120_000);
        }
        if let Ok(v) = std::env::var("DAEMON_PLUGIN_PTT_BINDING") {
            let v = v.trim();
            if !v.is_empty() {
                c.ptt_binding = v.into();
            }
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_PTT_MAX_HOLD_MS") {
            c.ptt_max_hold_ms = v.clamp(500, 600_000);
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_SAMPLE_RATE_HZ") {
            c.sample_rate_hz = v.clamp(8_000, 192_000) as u32;
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_CHUNK_MS") {
            c.chunk_ms = v.clamp(10, 1_000) as u32;
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_STREAM_ID") {
            c.stream_id = v.max(1) as i32;
        }
        if let Ok(v) = std::env::var("DAEMON_PLUGIN_TTS_PROVIDER") {
            let v = v.trim();
            if matches!(v, "sherpa" | "openai" | "elevenlabs") {
                c.tts_provider = v.into();
            }
        }
        if let Ok(v) = std::env::var("DAEMON_PLUGIN_TTS_VOICE") {
            if !v.trim().is_empty() {
                c.tts_voice = v.trim().into();
            }
        }
        if let Ok(v) = std::env::var("DAEMON_PLUGIN_TTS_FORMAT") {
            let v = v.trim();
            if matches!(
                v,
                "wav" | "mp3" | "pcm" | "opus" | "aac" | "flac" | "ulaw"
            ) {
                c.tts_format = v.into();
            }
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_MAX_STEPS") {
            c.max_steps = v.clamp(1, 16) as u32;
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_TIMEOUT_MS") {
            c.timeout_ms = v.max(500) as u32;
        }
        if let Some(v) = read_u64("DAEMON_PLUGIN_GOAL_TIMEOUT_MS") {
            c.goal_timeout_ms = v.max(1_000) as u32;
        }
        c
    }
}

/// One pending kernel-routed call handed from a task to the serve loop,
/// which sends it and correlates the `ActionResponse` by `action_id`.
pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// Cloneable handle for kernel-routed actions into other plugins
/// (`mic`, `stt`, `agent`, `tts`, `sound`). Every [`Rpc::call`] round-trips
/// through the serve loop's single `recv()` point.
#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self { tx }
    }

    /// One kernel-routed action round-trip. Resolves to the decoded
    /// `data_json` payload on `ACTION_OK`; transport failures, non-OK
    /// statuses and timeouts all surface as `Err` naming the target action.
    pub async fn call(
        &self,
        action: &str,
        params: Value,
        timeout_ms: u32,
    ) -> Result<Value, String> {
        let params_json = serde_json::to_vec(&params)
            .map_err(|e| format!("failed to encode {action} params: {e}"))?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RpcCall { action: action.to_string(), params_json, timeout_ms, reply })
            .await
            .map_err(|_| format!("{action} aborted: serve loop is shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("{action} aborted: serve loop is shutting down")),
            Err(_) => Err(format!("{action} timed out after {effective} ms")),
        }
    }
}

/// Loop/turn state shared between the serve loop, spawned handlers and the
/// timer task. Plain atomics + small mutex-guarded slots — no `.await` while
/// holding a guard.
#[derive(Default)]
pub struct DaemonState {
    enabled: AtomicBool,
    busy: AtomicBool,
    capturing: AtomicBool,
    turns_completed: AtomicU64,
    last_turn: Mutex<Option<Value>>,
    /// Short-term conversation memory (session lifetime): the last
    /// MEMORY_TURNS (prompt, answer) pairs, oldest first. Injected into each
    /// goal's `context` so follow-ups like "а теперь открой её" resolve.
    history: Mutex<VecDeque<(String, String)>>,
}

/// How many recent exchanges ride into the next goal's context.
const MEMORY_TURNS: usize = 5;

impl DaemonState {
    pub fn new(enabled_at_boot: bool) -> Self {
        Self { enabled: AtomicBool::new(enabled_at_boot), ..Default::default() }
    }

    /// Remember a finished exchange; evicts beyond [`MEMORY_TURNS`].
    pub fn remember_turn(&self, prompt: &str, answer: &str) {
        let mut h = self.history.lock().expect("history poisoned");
        h.push_back((prompt.to_string(), answer.to_string()));
        while h.len() > MEMORY_TURNS {
            h.pop_front();
        }
    }

    /// Pull (transcript, answer) out of a finished turn payload and store it.
    /// Silent/errored turns are not remembered — nothing worth recalling.
    pub fn remember_exchange(&self, transcript: &str, result: &Value) {
        let Some(ans) = result.get("answer").and_then(Value::as_str) else {
            return;
        };
        if ans.trim().is_empty() || transcript.trim().is_empty() {
            return;
        }
        self.remember_turn(transcript, ans);
    }

    /// Rendered context block for the next goal; `None` until the first
    /// exchange completes.
    pub fn recent_context(&self) -> Option<String> {
        let h = self.history.lock().expect("history poisoned");
        if h.is_empty() {
            return None;
        }
        let mut s = String::from("Недавний диалог с этим пользователем (старые снизу вверх):\n");
        for (prompt, answer) in h.iter() {
            s.push_str(&format!("Пользователь: {prompt}\nВин: {answer}\n"));
        }
        Some(s)
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    /// True while the mic is actually open (vad/ptt listen stages set this
    /// around their capture; `window` holds it for its whole fixed window).
    pub fn capturing(&self) -> bool {
        self.capturing.load(Ordering::SeqCst)
    }

    pub fn set_capturing(&self, on: bool) {
        self.capturing.store(on, Ordering::SeqCst);
    }

    /// Claim the single turn slot: `true` exactly once until [`Self::end_turn`].
    /// Both the timer tick and `daemon_turn` go through this, so a manual
    /// turn can't overlap the loop's turn (the mic has one owner).
    pub fn try_begin_turn(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn end_turn(&self, result: &Value) {
        self.turns_completed.fetch_add(1, Ordering::SeqCst);
        *self.last_turn.lock().expect("last_turn poisoned") = Some(result.clone());
        self.busy.store(false, Ordering::SeqCst);
    }

    pub fn snapshot(&self, mode: &str) -> Value {
        json!({
            "enabled": self.enabled(),
            "busy": self.busy.load(Ordering::SeqCst),
            "capturing": self.capturing(),
            "mode": mode,
            "turns_completed": self.turns_completed.load(Ordering::SeqCst),
            "last_turn":
                self.last_turn
                    .lock()
                    .expect("last_turn poisoned")
                    .clone(),
        })
    }
}

/// Best-effort event published AFTER the action response is sent (the kernel
/// namespaces the type to `plugin.daemon.turn.completed` / `.state.changed`).
#[derive(Debug)]
pub struct ChangeEvent {
    pub event_type: &'static str,
    pub payload: Value,
}

/// One handled action: the response payload plus an optional change event.
#[derive(Debug)]
pub struct ActionResult {
    pub data: Vec<u8>,
    pub event: Option<ChangeEvent>,
}

/// Handle one kernel-routed action. Stage failures inside `daemon_say` /
/// `daemon_ask` surface as `Err` → `ACTION_ERROR`; a voice turn always
/// reports through its result payload instead (`status: "error"`), because a
/// failed stage is a normal headless outcome (no speech, mic gone, agent
/// down), not a malformed request.
pub async fn handle_action(
    rpc: Rpc,
    state: std::sync::Arc<DaemonState>,
    config: &Config,
    action: &str,
    params_json: &[u8],
    bus: Option<&Bus>,
) -> Result<ActionResult, String> {
    match parse_request(action, params_json)? {
        DaemonRequest::Enable => {
            state.set_enabled(true);
            ok(json!({ "enabled": true }), Some(state_changed(true)))
        }
        DaemonRequest::Disable => {
            state.set_enabled(false);
            ok(json!({ "enabled": false }), Some(state_changed(false)))
        }
        DaemonRequest::Status => ok(state.snapshot(config.mode.as_str()), None),
        DaemonRequest::Turn { text } => {
            if !state.try_begin_turn() {
                return Err(
                    "ERR_DAEMON_BUSY: another voice turn is already in progress".into()
                );
            }
            let result = run_voice_turn(&rpc, state.clone(), config, text, bus).await;
            state.end_turn(&result);
            let event = ChangeEvent {
                event_type: "turn.completed",
                payload: result.clone(),
            };
            ok(result, Some(event))
        }
        DaemonRequest::Say { text } => {
            let spoken = speak(&rpc, config, &text).await?;
            ok(
                json!({
                    "spoken": true,
                    "clip_id": spoken["clip_id"],
                    "player": spoken["player"],
                    "format": config.tts_format,
                }),
                None,
            )
        }
        DaemonRequest::Ask { prompt } => {
            let answer = run_agent(&rpc, config, &prompt, "").await?;
            let mut spoken = false;
            if let Some(text) = answer.answer.as_deref() {
                speak(&rpc, config, text).await?;
                spoken = true;
            }
            ok(
                json!({
                    "answer": answer.answer,
                    "goal_id": answer.goal_id,
                    "goal_status": answer.goal_status,
                    "spoken": spoken,
                }),
                None,
            )
        }
    }
}

/// One listen→think→speak cycle. Never panics, never fails the caller: every
/// stage failure lands in the returned payload as `status: "error"` so the
/// background loop can run turns unattended forever.
///
/// The listen stage follows [`Config::mode`]: `window` holds the mic for a
/// fixed `turn_ms`; `vad` ends on stt's speech-ended event (falling back to
/// window behavior when no bus is available, i.e. in tests without a kernel
/// event feed); `ptt` falls back to the fixed window because a manual turn
/// has nobody holding the key.
pub async fn run_voice_turn(
    rpc: &Rpc,
    state: std::sync::Arc<DaemonState>,
    config: &Config,
    text_override: Option<String>,
    bus: Option<&Bus>,
) -> Value {
    let started = Instant::now();
    let transcript = match text_override {
        Some(t) => t,
        None => match listen(rpc, config, state.as_ref(), bus).await {
            Ok(t) => t,
            Err(e) => return turn_result("error", String::new(), None, false, started, Some(e)),
        },
    };
    let listen_ms = started.elapsed().as_millis() as u64;
    let memory_ctx = state.recent_context().unwrap_or_default();
    let result = respond(rpc, config, transcript.clone(), started, listen_ms, &memory_ctx).await;
    state.remember_exchange(&transcript, &result);
    result
}

/// Think+speak tail shared by every listen flavor: agent round-trip, then
/// speak the answer aloud. Empty transcripts short-circuit to `silent`.
/// `listen_ms` (capture+transcribe time) rides along into the `stages`
/// breakdown so operators can see where a slow turn spent its time.
pub async fn respond(
    rpc: &Rpc,
    config: &Config,
    transcript: String,
    started: Instant,
    listen_ms: u64,
    memory_ctx: &str,
) -> Value {
    if transcript.trim().is_empty() {
        let mut v = turn_result("silent", transcript, None, false, started, None);
        attach_stages(&mut v, listen_ms, 0, 0);
        return v;
    }

    let t_agent = Instant::now();
    let answer = match run_agent(rpc, config, &transcript, memory_ctx).await {
        Ok(a) => a,
        Err(e) => {
            let mut v =
                turn_result("error", transcript, None, false, started, Some(e));
            attach_stages(&mut v, listen_ms, t_agent.elapsed().as_millis() as u64, 0);
            return v;
        }
    };
    let agent_ms = t_agent.elapsed().as_millis() as u64;

    // A goal can legitimately finish without prose (declined, needs
    // confirmation, max_steps): report it rather than speaking nothing
    // silently.
    let Some(text) = answer.answer.clone() else {
        let mut v = turn_result_with_goal(
            "error",
            transcript,
            false,
            started,
            Some(format!(
                "agent finished without an answer (status: {})",
                answer.goal_status
            )),
            &answer,
        );
        attach_stages(&mut v, listen_ms, agent_ms, 0);
        return v;
    };

    let t_speak = Instant::now();
    match speak(rpc, config, &text).await {
        Ok(_) => {}
        Err(e) => {
            let mut v =
                turn_result_with_goal("error", transcript, false, started, Some(e), &answer);
            attach_stages(&mut v, listen_ms, agent_ms, t_speak.elapsed().as_millis() as u64);
            return v;
        }
    };
    let speak_ms = t_speak.elapsed().as_millis() as u64;

    let mut v = turn_result_with_goal("answered", transcript, true, started, None, &answer);
    attach_stages(&mut v, listen_ms, agent_ms, speak_ms);
    v
}

/// Attach the per-stage latency breakdown and echo it to the plugin log —
/// one line per turn is the whole observability story for voice latency.
fn attach_stages(v: &mut Value, listen_ms: u64, agent_ms: u64, speak_ms: u64) {
    let total = v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "stages".into(),
            json!({"listen_ms": listen_ms, "agent_ms": agent_ms, "speak_ms": speak_ms}),
        );
    }
    eprintln!(
        "[daemon] turn stages: listen={listen_ms}ms agent={agent_ms}ms speak={speak_ms}ms total={total}ms"
    );
}

/// Dispatch the listen stage by mode.
async fn listen(
    rpc: &Rpc,
    config: &Config,
    state: &DaemonState,
    bus: Option<&Bus>,
) -> Result<String, String> {
    match (config.mode, bus) {
        (ListenMode::Vad, Some(bus)) => {
            let session = start_capture(rpc, config).await?;
            state.set_capturing(true);
            let end = wait_for_speech_end(config, bus).await;
            state.set_capturing(false);
            let _ = end;
            finish_capture(rpc, config, session).await
        }
        _ => listen_window(rpc, config, state).await,
    }
}

/// `window` mode listen: open a capture, hold it for the fixed window, stop.
async fn listen_window(rpc: &Rpc, config: &Config, state: &DaemonState) -> Result<String, String> {
    let session = start_capture(rpc, config).await?;
    state.set_capturing(true);
    tokio::time::sleep(std::time::Duration::from_millis(config.turn_ms)).await;
    let transcript = finish_capture(rpc, config, session).await;
    state.set_capturing(false);
    transcript
}

/// One mic capture session: an stt accumulation buffer plus the recorder
/// pointed at it. Dropping without [`finish_capture`] leaks the buffer —
/// always run the pair.
struct CaptureSession {
    session_id: String,
}

/// Open an stt accumulation buffer and start the mic aimed at it. Fails
/// closed: if `mic_start` errors after `stt_listen_start` succeeded, the
/// buffer is discarded before returning so no stale stream survives.
async fn start_capture(rpc: &Rpc, config: &Config) -> Result<CaptureSession, String> {
    rpc.call(
        "stt_listen_start",
        json!({
            "stream_id": config.stream_id,
            "sample_rate_hz": config.sample_rate_hz,
            "num_channels": 1,
        }),
        config.timeout_ms,
    )
    .await?;

    let mic = rpc.call(
        "mic_start",
        json!({
            "target": STT_TARGET,
            "stream_id": config.stream_id,
            "sample_rate_hz": config.sample_rate_hz,
            "num_channels": 1,
            "chunk_ms": config.chunk_ms,
        }),
        config.timeout_ms,
    )
    .await;
    let mic = match mic {
        Ok(m) => m,
        Err(e) => {
            let _ = rpc
                .call(
                    "stt_listen_stop",
                    json!({ "stream_id": config.stream_id }),
                    config.timeout_ms,
                )
                .await;
            return Err(e);
        }
    };
    let session_id = mic["session_id"]
        .as_str()
        .ok_or_else(|| "mic_start returned no session_id".to_string())?
        .to_string();
    Ok(CaptureSession { session_id })
}

/// Close a capture and transcribe: `mic_stop` first — it flushes
/// `end_of_stream` to stt — then `stt_listen_stop` over the complete buffer.
/// The ordering below is load-bearing.
async fn finish_capture(rpc: &Rpc, config: &Config, session: CaptureSession) -> Result<String, String> {
    rpc.call("mic_stop", json!({ "session_id": session.session_id }), config.timeout_ms).await?;

    let stop = rpc
        .call(
            "stt_listen_stop",
            json!({ "stream_id": config.stream_id }),
            config.timeout_ms,
        )
        .await?;
    stop["text"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "stt_listen_stop returned no text".to_string())
}

/// What ended a vad-mode capture.
#[derive(Debug, PartialEq, Eq)]
enum SpeechEnd {
    /// stt reported the utterance boundary — the happy path.
    Ended,
    /// No speech within `vad_wait_ms`.
    Silent,
    /// Speech ran past `vad_max_utterance_ms` with no ending event.
    MaxUtterance,
}

/// Await the vad-mode endpoint off the bus: subscribe BEFORE any capture
/// starts so events racing the mic open are retained by the broadcast
/// channel's backlog.
async fn wait_for_speech_end(config: &Config, bus: &Bus) -> SpeechEnd {
    let mut rx = bus.subscribe();
    let mut speaking = false;
    let mut idle_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(config.vad_wait_ms);
    loop {
        let until_utterance_cap =
            tokio::time::Instant::now() + std::time::Duration::from_millis(config.vad_max_utterance_ms);
        let deadline = if speaking { until_utterance_cap } else { idle_deadline };
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return if speaking { SpeechEnd::MaxUtterance } else { SpeechEnd::Silent };
            }
            ev = rx.recv() => match ev {
                Ok((etype, payload)) => {
                    let sid = payload.get("stream_id").and_then(Value::as_i64);
                    let ours = sid == Some(config.stream_id as i64);
                    if !ours {
                        continue;
                    }
                    match etype.as_str() {
                        EV_SPEECH_STARTED if !speaking => {
                            speaking = true;
                            idle_deadline = tokio::time::Instant::now()
                                + std::time::Duration::from_millis(config.vad_wait_ms);
                        }
                        EV_SPEECH_ENDED => return SpeechEnd::Ended,
                        _ => {}
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return SpeechEnd::Silent,
            },
        }
    }
}

/// Push-to-talk turn: capture opens when the key went down and closes on
/// release (or the max-hold cap). Runs inside the ptt task with its own bus
/// receiver, so release events can't be missed while the mic opens.
pub async fn run_ptt_turn(
    rpc: &Rpc,
    config: &Config,
    state: &DaemonState,
    rx: &mut tokio::sync::broadcast::Receiver<(String, Value)>,
) -> Value {
    let started = Instant::now();
    let transcript = match ptt_listen(rpc, config, rx).await {
        Ok(t) => t,
        Err(e) => return turn_result("error", String::new(), None, false, started, Some(e)),
    };
    let listen_ms = started.elapsed().as_millis() as u64;
    let memory_ctx = state.recent_context().unwrap_or_default();
    let result = respond(rpc, config, transcript.clone(), started, listen_ms, &memory_ctx).await;
    state.remember_exchange(&transcript, &result);
    result
}

/// The long-lived push-to-talk worker (`ptt` mode): idles on the hotkey
/// event stream, claims the single turn slot on a matching press, captures
/// until release, then thinks+speaks like any other turn. Exits only when
/// the bus closes (kernel gone).
pub async fn ptt_task(
    rpc: Rpc,
    state: std::sync::Arc<DaemonState>,
    config: Config,
    bus: Bus,
    outbound: tokio::sync::mpsc::Sender<Envelope>,
) {
    let mut rx = bus.subscribe();
    loop {
        let (etype, payload) = match rx.recv().await {
            Ok(ev) => ev,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        };
        if etype != EV_HOTKEY_PRESSED || !state.enabled() {
            continue;
        }
        if payload.get("binding").and_then(Value::as_str)
            != Some(config.ptt_binding.as_str())
        {
            continue;
        }
        if !state.try_begin_turn() {
            // A manual turn holds the mic; the keypress is dropped rather
            // than queued — pressing again once idle works.
            continue;
        }
        state.set_capturing(true);
        let result = run_ptt_turn(&rpc, &config, &state, &mut rx).await;
        state.set_capturing(false);
        state.end_turn(&result);
        let ev = ChangeEvent { event_type: "turn.completed", payload: result };
        let _ = outbound.send(event_envelope(&ev)).await;
    }
}

/// The ptt listen stage: open the capture, wait for the key release (the
/// same stream_id filter as vad mode), close, transcribe.
async fn ptt_listen(
    rpc: &Rpc,
    config: &Config,
    rx: &mut tokio::sync::broadcast::Receiver<(String, Value)>,
) -> Result<String, String> {
    let session = start_capture(rpc, config).await?;
    let released = loop {
        match tokio::time::timeout(
            std::time::Duration::from_millis(config.ptt_max_hold_ms),
            rx.recv(),
        )
        .await
        {
            Ok(Ok((etype, payload))) => {
                let ours = payload.get("binding").and_then(Value::as_str)
                    == Some(config.ptt_binding.as_str());
                if etype == EV_HOTKEY_RELEASED && ours {
                    break Ok(());
                }
                // Other events (stt chatter, unrelated bindings) don't end
                // the hold; keep waiting.
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            // Bus closed (kernel gone) or hold cap hit — either way stop
            // capturing and take whatever was said so far.
            _ => break Err("hotkey release never arrived (max hold elapsed)".to_string()),
        }
    };
    let transcript = finish_capture(rpc, config, session).await;
    released.and(transcript)
}

/// The think stage: hand the prompt to the agent's goal loop. (Error strings
/// already name the action — the serve loop prefixes `{action} failed:` on
/// non-OK replies and [`Rpc::call`] names it on transport failures.)
async fn run_agent(
    rpc: &Rpc,
    config: &Config,
    prompt: &str,
    memory_ctx: &str,
) -> Result<AgentAnswer, String> {
    let mut body = json!({ "goal": prompt, "max_steps": config.max_steps });
    if !memory_ctx.is_empty() {
        body["context"] = json!(memory_ctx);
    }
    let goal = rpc
        .call("goal_start", body, config.goal_timeout_ms)
        .await?;
    Ok(AgentAnswer {
        goal_id: goal["id"].as_str().unwrap_or_default().to_string(),
        goal_status: goal["status"].as_str().unwrap_or_default().to_string(),
        answer: goal["final_answer"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string),
    })
}

struct AgentAnswer {
    goal_id: String,
    goal_status: String,
    answer: Option<String>,
}

/// The speak stage: synthesize then play. Returns `sound_play`'s response.
async fn speak(rpc: &Rpc, config: &Config, text: &str) -> Result<Value, String> {
    let synth = rpc
        .call(
            "tts_synthesize",
            json!({
                "provider": config.tts_provider,
                "voice": config.tts_voice,
                "format": config.tts_format,
                "text": text,
            }),
            config.timeout_ms,
        )
        .await?;
    let audio_base64 = synth["audio_base64"]
        .as_str()
        .ok_or_else(|| "tts_synthesize returned no audio_base64".to_string())?;
    let format = synth["format"].as_str().unwrap_or(&config.tts_format);

    rpc.call(
        "sound_play",
        json!({ "data_base64": audio_base64, "format": format }),
        config.timeout_ms,
    )
    .await
}

fn turn_result(
    status: &str,
    transcript: String,
    answer: Option<String>,
    spoken: bool,
    started: Instant,
    error: Option<String>,
) -> Value {
    json!({
        "status": status,
        "transcript": transcript,
        "answer": answer,
        "spoken": spoken,
        "duration_ms": started.elapsed().as_millis() as u64,
        "error": error,
    })
}

fn turn_result_with_goal(
    status: &str,
    transcript: String,
    spoken: bool,
    started: Instant,
    error: Option<String>,
    answer: &AgentAnswer,
) -> Value {
    json!({
        "status": status,
        "transcript": transcript,
        "answer": answer.answer,
        "goal_id": answer.goal_id,
        "goal_status": answer.goal_status,
        "spoken": spoken,
        "duration_ms": started.elapsed().as_millis() as u64,
        "error": error,
    })
}

fn ok(data: Value, event: Option<ChangeEvent>) -> Result<ActionResult, String> {
    let data =
        serde_json::to_vec(&data).map_err(|e| format!("failed to encode response: {e}"))?;
    Ok(ActionResult { data, event })
}

fn state_changed(enabled: bool) -> ChangeEvent {
    ChangeEvent {
        event_type: "state.changed",
        payload: json!({ "enabled": enabled }),
    }
}

/// Build the outbound `EventPublish` envelope for a change event.
pub fn event_envelope(event: &ChangeEvent) -> Envelope {
    Envelope {
        payload: Some(envelope::Payload::EventPublish(EventPublish {
            event_type: event.event_type.to_string(),
            payload_json: event.payload.to_string().into_bytes(),
        })),
        ..Default::default()
    }
}
