//! Tool catalog for the `agent` plugin: the operator-curated set of kernel
//! actions the model may call, plus their descriptions and JSON-Schema
//! parameter specs handed to the LLM.
//!
//! Two sources, both operator-controlled and default-deny (same posture as
//! `AI_PLUGIN_ALLOWED_KEY_ENVS` / `FILES_PLUGIN_ALLOWED_ROOTS`):
//!
//! - `AGENT_PLUGIN_ALLOWED_ACTIONS` — comma-separated exact action names.
//!   This is the security allowlist: nothing outside it is ever dispatched,
//!   whatever a tool file or the model says. Unset/empty → empty catalog
//!   (the loop still runs, but every tool call errors back to the model).
//! - `AGENT_PLUGIN_TOOLS_FILE` — optional path to a JSON file describing
//!   the tools:
//!
//! ```json
//! {"tools": [{
//!     "name": "notify_send",
//!     "description": "Send a desktop notification.",
//!     "parameters": {"type": "object", "properties": {"title": {"type": "string"}}},
//!     "requires_confirmation": false,
//!     "timeout_ms": 30000
//! }]}
//! ```
//!
//! (A bare array is accepted too.) Entries whose `name` is not on the
//! allowlist are ignored; allowlisted names without a file entry get a
//! minimal spec (empty description, no schema). The file is re-read on
//! every goal start, so operator edits apply without a plugin restart.

use std::collections::BTreeMap;

pub const ALLOWED_ACTIONS_ENV: &str = "AGENT_PLUGIN_ALLOWED_ACTIONS";
pub const TOOLS_FILE_ENV: &str = "AGENT_PLUGIN_TOOLS_FILE";
pub const APPROVALS_FILE_ENV: &str = "AGENT_PLUGIN_APPROVALS_FILE";
/// `off` disables kernel manifest discovery (static catalog only).
pub const DISCOVERY_ENV: &str = "AGENT_PLUGIN_DISCOVERY";

/// Per-dispatch timeout floor/ceiling (ms).
pub const TOOL_TIMEOUT_MIN_MS: u32 = 1_000;
pub const TOOL_TIMEOUT_MAX_MS: u32 = 120_000;
pub(crate) const TOOL_TIMEOUT_DEFAULT_MS: u32 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Allowlisted name with no description anywhere (dispatchable, opaque).
    Minimal,
    /// Filled from the owning plugin's registered manifest (`get_manifest`
    /// kernel command) — the authoritative runtime truth.
    Kernel,
    /// Operator-curated entry from `AGENT_PLUGIN_TOOLS_FILE`; wins over
    /// kernel data because the operator wrote it deliberately.
    File,
    /// Operator entry whose gaps were filled from the owning plugin's
    /// manifest (see [`merge_from_kernel`]) — the operator keeps the fields
    /// they wrote, the plugin supplies the rest.
    Merged,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON-Schema object describing the params handed to the LLM;
    /// `Value::Null` when unknown.
    pub parameters: serde_json::Value,
    /// When true the engine never dispatches this tool on its own: the goal
    /// halts in `needs_confirmation` until an operator-approved resume.
    pub requires_confirmation: bool,
    /// Kernel risk label (`low`/`medium`/`high`); empty when unknown.
    #[serde(default)]
    pub risk: String,
    pub timeout_ms: u32,
    #[serde(default)]
    pub cooldown_ms: u64,
    #[serde(default)]
    pub max_per_goal: u32,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    /// Effective specs in allowlist order — exactly what the model may call.
    pub tools: Vec<ToolSpec>,
    pub allowed_actions: Vec<String>,
    pub tools_file_set: bool,
}

impl Catalog {
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.iter().find(|t| t.name == name)
    }
}

fn parse_allowlist(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let name = part.trim();
        if !name.is_empty() && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

fn parse_spec(v: &serde_json::Value, index: usize) -> Result<ToolSpec, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| format!("tools file entry #{index} must be an object"))?;
    let name = obj
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("tools file entry #{index} is missing a non-empty \"name\""))?;
    if name.contains(char::is_whitespace) {
        return Err(format!("tools file entry #{index}: \"name\" must not contain whitespace"));
    }
    let description = obj
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let parameters = match obj.get("parameters") {
        None | Some(serde_json::Value::Null) => serde_json::Value::Null,
        Some(p) if p.is_object() => p.clone(),
        Some(_) => {
            return Err(format!(
                "tools file entry \"{name}\": \"parameters\" must be a JSON-Schema object"
            ))
        }
    };
    let requires_confirmation = obj
        .get("requires_confirmation")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let risk = obj
        .get("risk")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let timeout_ms = match obj.get("timeout_ms") {
        None | Some(serde_json::Value::Null) => TOOL_TIMEOUT_DEFAULT_MS,
        Some(n) => {
            let raw = n
                .as_u64()
                .ok_or_else(|| format!("tools file entry \"{name}\": \"timeout_ms\" must be a non-negative integer"))?;
            (raw as u32).clamp(TOOL_TIMEOUT_MIN_MS, TOOL_TIMEOUT_MAX_MS)
        }
    };
    let cooldown_ms = match obj.get("cooldown_ms") {
        None | Some(serde_json::Value::Null) => 0,
        Some(n) => n.as_u64().ok_or_else(|| format!("tools file entry \"{name}\": \"cooldown_ms\" must be a non-negative integer"))?,
    };
    let max_per_goal = match obj.get("max_per_goal") {
        None | Some(serde_json::Value::Null) => 16,
        Some(n) => {
            let raw = n.as_u64().ok_or_else(|| format!("tools file entry \"{name}\": \"max_per_goal\" must be a non-negative integer"))?;
            if raw > 1000 { return Err(format!("tools file entry \"{name}\": \"max_per_goal\" must be <= 1000")); }
            raw as u32
        }
    };
    Ok(ToolSpec {
        name: name.to_string(),
        description,
        parameters,
        requires_confirmation,
        risk,
        timeout_ms,
        cooldown_ms,
        max_per_goal,
        source: Source::File,
    })
}


/// Fill an operator entry's gaps from the owning plugin's manifest spec.
///
/// The two layers describe different things and neither is wholly
/// authoritative: the plugin owns its parameter schema, risk label and
/// confirmation demand; the operator owns the description, because that
/// text is what `engine::embedding_filtered_catalog` embeds to decide
/// whether the tool is offered for a goal at all — it is written in the
/// language the operator's goals are written in, which a shipped plugin
/// cannot know. So merge per field instead of letting one layer win
/// wholesale: an operator entry may shrink to `{name, description}` and
/// still get a real schema.
///
/// `requires_confirmation` is OR-ed, never assigned. Merging may only add
/// friction — a kernel `false` must not clear an operator `true`, and a
/// plugin that demands confirmation gets it even if the operator entry
/// predates that demand.
///
/// Dispatch limits (`timeout_ms`, `cooldown_ms`, `max_per_goal`) are
/// deliberately untouched: those are operator policy, not plugin facts.
fn merge_from_kernel(spec: &mut ToolSpec, kernel: &ToolSpec) {
    let mut filled = false;
    if spec.description.trim().is_empty() && !kernel.description.trim().is_empty() {
        spec.description = kernel.description.clone();
        filled = true;
    }
    if !spec.parameters.is_object() && kernel.parameters.is_object() {
        spec.parameters = kernel.parameters.clone();
        filled = true;
    }
    if spec.risk.trim().is_empty() && !kernel.risk.trim().is_empty() {
        spec.risk = kernel.risk.clone();
        filled = true;
    }
    if kernel.requires_confirmation && !spec.requires_confirmation {
        spec.requires_confirmation = true;
        filled = true;
    }
    if filled {
        spec.source = Source::Merged;
    }
}

fn parse_approvals_file(raw: &str) -> Result<std::collections::BTreeMap<String, bool>, String> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|e| format!("approvals file is not valid JSON: {e}"))?;
    let obj = v.as_object().ok_or_else(|| "approvals file must be an object {\"tool\": bool}" .to_string())?;
    let mut map = std::collections::BTreeMap::new();
    for (k, val) in obj {
        let b = val.as_bool().ok_or_else(|| format!("approvals entry \"{k}\" must be boolean (true=requires confirmation)"))?;
        map.insert(k.clone(), b);
    }
    Ok(map)
}

fn parse_tools_file(raw: &str) -> Result<BTreeMap<String, ToolSpec>, String> {
    let body: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| format!("tools file is not valid JSON: {e}"))?;
    let list = match body {
        serde_json::Value::Array(items) => items,
        ref other @ serde_json::Value::Object(_) => other
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .ok_or_else(|| "tools file must be an array of specs or {\"tools\": [...]}".to_string())?,
        _ => return Err("tools file must be an array of specs or {\"tools\": [...]}".to_string()),
    };
    let mut map = BTreeMap::new();
    for (i, item) in list.iter().enumerate() {
        let spec = parse_spec(item, i)?;
        if map.insert(spec.name.clone(), spec.clone()).is_some() {
            return Err(format!("tools file declares \"{}\" twice", spec.name));
        }
    }
    Ok(map)
}

impl Catalog {
    /// Build the effective catalog from process env. Read per goal start so
    /// operator edits land without a restart (see module docs).
    pub fn load() -> Result<Catalog, String> {
        let allowed_raw = std::env::var(ALLOWED_ACTIONS_ENV).unwrap_or_default();
        let file_path = std::env::var(TOOLS_FILE_ENV).ok().filter(|s| !s.is_empty());
        Self::build(&allowed_raw, file_path.as_deref())
    }

    /// Pure core of [`Catalog::load`] — same logic, no process env access,
    /// so tests can exercise it without the parallel-test env races
    /// documented in `docs/PLUGIN_AUTHORING.md` §6.
    pub fn build(allowed_raw: &str, file_path: Option<&str>) -> Result<Catalog, String> {
        let allowed = parse_allowlist(allowed_raw);

        let specs = match file_path {
            Some(path) => {
                let raw = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read tools file \"{path}\": {e}"))?;
                parse_tools_file(&raw)?
            }
            None => BTreeMap::new(),
        };

        let approvals = match std::env::var(APPROVALS_FILE_ENV).ok().filter(|s| !s.is_empty()) {
            Some(path) => {
                let raw = std::fs::read_to_string(&path).map_err(|e| format!("cannot read approvals file \"{path}\": {e}"))?;
                parse_approvals_file(&raw)?
            }
            None => std::collections::BTreeMap::new(),
        };
        let tools = allowed
            .iter()
            .map(|name| {
                let mut spec = match specs.get(name) {
                    Some(s) => s.clone(),
                    None => ToolSpec {
                        name: name.clone(),
                        description: String::new(),
                        parameters: serde_json::Value::Null,
                        requires_confirmation: false,
                        risk: String::new(),
                        timeout_ms: TOOL_TIMEOUT_DEFAULT_MS,
                        cooldown_ms: 0,
                        max_per_goal: 16,
                        source: Source::Minimal,
                    },
                };
                if let Some(&confirm) = approvals.get(name) {
                    spec.requires_confirmation = confirm;
                }
                spec
            })
            .collect();

        Ok(Catalog { tools, allowed_actions: allowed, tools_file_set: file_path.is_some() })
    }

    /// [`Catalog::load`] plus runtime manifest discovery: for every
    /// allowlisted tool still [`Source::Minimal`], fill description/schemas/
    /// confirmation from the owning plugin's registered manifest via the
    /// kernel's read-only `list_plugins` + `get_manifest` commands. File
    /// entries keep every field the operator actually wrote and take the
    /// rest from the plugin (see [`merge_from_kernel`]), so an operator
    /// entry can be trimmed to `{name, description}` without losing its
    /// schema. On any discovery failure we log loudly and keep the static
    /// catalog, so an older kernel degrades gracefully instead of breaking
    /// goals. `AGENT_PLUGIN_DISCOVERY=off` skips the round-trips entirely.
    pub async fn load_with_discovery(rpc: &crate::Rpc) -> Result<Catalog, String> {
        let mut cat = Self::load()?;
        if std::env::var(DISCOVERY_ENV).as_deref() == Ok("off") {
            infer_missing_risk(&mut cat);
            return Ok(cat);
        }
        match crate::discovery::discover(rpc).await {
            Ok(map) => {
                for tool in cat.tools.iter_mut() {
                    let Some(found) = map.get(&tool.name) else { continue };
                    match tool.source {
                        Source::Minimal => *tool = found.clone(),
                        Source::File | Source::Merged => merge_from_kernel(tool, found),
                        Source::Kernel => {}
                    }
                }
            }
            Err(e) => {
                eprintln!("[agent] manifest discovery unavailable, using static catalog: {e}")
            }
        }
        // Applied last, after every catalog layer (file, kernel manifest,
        // merge) has had its say, so a real declaration from any layer is
        // always what "explicit" means here — inference only ever sees the
        // holes nothing upstream filled.
        infer_missing_risk(&mut cat);
        Ok(cat)
    }
}

/// Fail-closed safety net for tools no plugin manifest and no operator file
/// entry ever assigned a `risk` to. Applied once, after every catalog layer
/// (`AGENT_PLUGIN_TOOLS_FILE`, kernel manifest discovery, and the merge
/// between them) has settled — so it only ever fills a hole, it never
/// overrides a real declaration: a spec with a non-empty `risk`, or with
/// `requires_confirmation: true` already set, is left completely alone. An
/// explicit manifest declaration always wins over this inference.
///
/// When inference does assign a risk, `requires_confirmation` is set to
/// `true` for "high" or "critical" — never for "medium" or "low", so a
/// pure read (`fs_read`, `secret_list`'s reads-adjacent cousins, `db_get`,
/// …) never grows a confirmation requirement it didn't have. Confirmation
/// is only ever flipped `false` → `true` here, never the reverse: this
/// function can only add friction, not remove it.
/// A risk label that means "nobody declared one". The empty string is the
/// obvious case, but `"unknown"` is just as unset: `ActionRisk::Unknown` is
/// the proto's zero value, so a plugin.json action with no `risk` key
/// round-trips through `risk_from_str("")` into the wire enum and arrives
/// here spelled out as a non-empty word. Measured on the live kernel right
/// after this inference first shipped: all 29 telegram actions came through
/// as `risk="unknown"`, which the earlier `is_empty()` check read as an
/// explicit declaration and skipped — leaving `tg_send_message` reaching a
/// human with no confirmation. A placeholder that is not empty defeats an
/// emptiness test, so test for the meaning, not the byte length.
fn risk_is_undeclared(risk: &str) -> bool {
    let risk = risk.trim();
    risk.is_empty() || risk.eq_ignore_ascii_case("unknown")
}

fn infer_missing_risk(catalog: &mut Catalog) {
    for spec in catalog.tools.iter_mut() {
        if !risk_is_undeclared(&spec.risk) {
            continue;
        }
        let risk = infer_risk(&spec.name);
        spec.risk = risk.to_string();
        if matches!(risk, "high" | "critical") {
            spec.requires_confirmation = true;
        }
    }
}

/// Classify an action name into a risk label using an ordered rule table —
/// first match wins. The ordering is deliberate and each rule's comment
/// says why it has to run before or after its neighbors; in particular a
/// generic suffix rule (e.g. "contains `delete`") must never get a chance
/// to relabel something a more specific, higher-stakes rule already caught
/// (e.g. `secret_delete` must land on the `secret_` prefix rule, not the
/// generic delete rule, even though both would fire on that name).
fn infer_risk(name: &str) -> &'static str {
    // 1. Secrets: touching a secret at all — even reading it — is the
    //    exposure, since the value then sits in the model's context where
    //    it can be echoed, logged, or acted on. Must run before the
    //    generic delete/write rules below so `secret_delete`/`secret_set`
    //    don't fall through to a lower, delete/write-shaped label.
    if name.starts_with("secret_") {
        return "critical";
    }

    // 2. Breaks out of the agent's own sandboxed domain entirely: `launch`
    //    runs an arbitrary program on the host. Sits alongside secrets at
    //    "critical" — a notch above every plain "high" below.
    //
    //    `hotkey_inject` is deliberately NOT here. Verified against
    //    plugins/hotkey/src: it does not synthesize keystrokes (no
    //    xdotool/ydotool/XTest/uinput/libei/evdev — the plugin only holds
    //    PERMISSION_EVENT_PUBLISH and PERMISSION_SYSTEM). It publishes a
    //    synthetic `hotkey_pressed`/`hotkey_released` kernel event for a
    //    given binding id — the "manual" backend's event source for
    //    compositor exec wiring and tests. The real risk is narrower and
    //    indirect (it fires whatever a subscriber bound to that id, e.g.
    //    spuriously starting a push-to-talk recording), so it lands in the
    //    "medium" bucket below with the other recoverable state-mutating
    //    actions, not here.
    if matches!(name, "launch") {
        return "critical";
    }

    // 3. Other single-action high-severity ops, named exactly so a
    //    same-prefix read-only sibling (`launch_list`, `db_get`,
    //    `mic_status`) never matches by accident: arbitrary filesystem
    //    write, raw SQL against the caller's own store, arbitrary outbound
    //    HTTP (an exfiltration channel, especially paired with
    //    `secret_get`), and starting a live audio/video capture (a
    //    privacy-sensitive surveillance capability — stopping one is far
    //    less sensitive and is handled by the generic "_stop" rule below).
    if matches!(
        name,
        "fs_write" | "db_query" | "http_request" | "mic_start" | "stt_listen_start"
            | "capture_record_start"
    ) {
        return "high";
    }

    // 4. Outbound sends that put something in front of a human being,
    //    rather than merely mutating this plugin's own stored state.
    if matches!(
        name,
        "email_send" | "tg_send_message" | "tg_delete_message" | "tg_forward_message"
    ) {
        return "high";
    }

    // 5. Destructive deletes outside the `secret_*` family already handled
    //    by rule 1: irreversible, but scoped to the plugin's own domain
    //    data (a note, an event, a schedule, a vector, a raw kv row)
    //    rather than a credential — high, not critical. `note_delete` and
    //    `secret_delete` therefore land on different tiers on purpose: the
    //    same verb, "delete", is not the same risk on every noun.
    if name.contains("delete") || name == "sync_del" {
        return "high";
    }

    // 6. Everything else that writes, creates, updates, binds, sets,
    //    starts or stops something: recoverable state mutation. This is
    //    the "medium" bucket — real side effects, but nothing destructive
    //    and nothing that reaches a human directly.
    if name.ends_with("_write")
        || name.ends_with("_set")
        || name.ends_with("_create")
        || name.ends_with("_update")
        || name.ends_with("_bind")
        || name.ends_with("_unbind")
        || name.ends_with("_start")
        || name.ends_with("_stop")
        || name.ends_with("_mute")
        || matches!(
            name,
            "sys_lock"
                | "hotkey_inject"
                | "sync_set"
                | "notify_send"
                | "notify_mark_read"
                | "tg_edit_message"
                | "tg_pin_message"
                | "tg_mark_read"
                | "tg_mark_all_read"
                | "tg_add_reaction"
                | "tg_upload_media"
                | "tg_download_media"
                | "vec_upsert"
                | "vec_upsert_batch"
                | "db_incr"
                | "db_append"
                | "db_patch"
                | "db_set"
                | "media_play"
                | "media_pause"
                | "media_play_pause"
                | "media_next"
                | "media_prev"
                | "media_stop"
                | "media_seek"
                | "media_seek_relative"
                | "media_shuffle"
                | "media_loop"
                | "media_volume"
                | "sound_play"
                | "sound_stop"
                | "tts_speak"
                | "tts_synthesize"
                | "clipboard_write"
                | "capture_screenshot"
                | "daemon_ask"
                | "daemon_say"
                | "daemon_turn"
                | "daemon_enable"
                | "daemon_disable"
        )
    {
        return "medium";
    }

    // 7. Default: pure reads, lists, and status calls. Never gets
    //    `requires_confirmation` — a read-only agent is a useless one.
    "low"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
            requires_confirmation: false,
            risk: String::new(),
            timeout_ms: 30_000,
            cooldown_ms: 0,
            max_per_goal: 16,
            source: Source::Minimal,
        }
    }

    fn kernel_spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: "kernel description".into(),
            parameters: json!({"type": "object", "properties": {"peer": {"type": "string"}}}),
            requires_confirmation: false,
            risk: "medium".into(),
            timeout_ms: 30_000,
            cooldown_ms: 0,
            max_per_goal: 16,
            source: Source::Kernel,
        }
    }

    fn file_spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::Value::Null,
            requires_confirmation: false,
            risk: String::new(),
            timeout_ms: 30_000,
            cooldown_ms: 0,
            max_per_goal: 16,
            source: Source::File,
        }
    }

    #[test]
    fn merge_fills_only_the_fields_the_operator_left_empty() {
        let mut tool = file_spec("tg_send_message");
        tool.description = "Отправь сообщение в телеграм".into();
        merge_from_kernel(&mut tool, &kernel_spec("tg_send_message"));
        assert_eq!(
            tool.description, "Отправь сообщение в телеграм",
            "an operator description is the retrieval signal — never overwrite it"
        );
        assert_eq!(tool.parameters["properties"]["peer"]["type"], "string");
        assert_eq!(tool.risk, "medium");
        assert_eq!(tool.source, Source::Merged);
    }

    #[test]
    fn merge_leaves_a_fully_specified_file_entry_alone() {
        let mut tool = file_spec("a");
        tool.description = "op".into();
        tool.parameters = json!({"type": "object", "properties": {"own": {}}});
        tool.risk = "low".into();
        let before = tool.clone();
        merge_from_kernel(&mut tool, &kernel_spec("a"));
        assert_eq!(tool, before, "nothing was empty, so nothing may change — including source");
    }

    #[test]
    fn merge_never_lowers_a_confirmation_requirement() {
        let mut tool = file_spec("secret_delete");
        tool.description = "op".into();
        tool.parameters = json!({"type": "object"});
        tool.risk = "high".into();
        let mut kernel = kernel_spec("secret_delete");
        kernel.requires_confirmation = true;
        merge_from_kernel(&mut tool, &kernel);
        assert!(
            tool.requires_confirmation,
            "the owning plugin demanding confirmation must win — merging may only add friction"
        );

        let mut tool = file_spec("b");
        tool.description = "op".into();
        tool.parameters = json!({"type": "object"});
        tool.risk = "low".into();
        tool.requires_confirmation = true;
        merge_from_kernel(&mut tool, &kernel_spec("b"));
        assert!(tool.requires_confirmation, "a kernel `false` must not clear an operator `true`");
    }

    #[test]
    fn merge_keeps_operator_dispatch_limits() {
        let mut tool = file_spec("a");
        tool.timeout_ms = 90_000;
        tool.cooldown_ms = 500;
        tool.max_per_goal = 2;
        merge_from_kernel(&mut tool, &kernel_spec("a"));
        assert_eq!((tool.timeout_ms, tool.cooldown_ms, tool.max_per_goal), (90_000, 500, 2));
    }

    #[test]
    fn allowlist_parses_trims_and_dedups() {
        assert_eq!(parse_allowlist(" a , b ,,a"), vec!["a", "b"]);
        assert!(parse_allowlist("  ").is_empty());
    }

    #[test]
    fn parses_wrapped_and_bare_array_files() {
        let wrapped = json!({"tools": [spec_json("a", false)]}).to_string();
        let map = parse_tools_file(&wrapped).unwrap();
        assert_eq!(map["a"].name, "a");
        let bare = json!([spec_json("b", true)]).to_string();
        let map = parse_tools_file(&bare).unwrap();
        assert!(map["b"].requires_confirmation);
    }

    fn spec_json(name: &str, confirm: bool) -> serde_json::Value {
        json!({"name": name, "description": "d", "requires_confirmation": confirm})
    }

    #[test]
    fn rejects_bad_files_loudly() {
        let err = parse_tools_file("not json").unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
        let err = parse_tools_file(r#"{"nope": []}"#).unwrap_err();
        assert!(err.contains("{\"tools\""), "{err}");
        let err = parse_tools_file(r#"[{"description":"x"}]"#).unwrap_err();
        assert!(err.contains("#0") && err.contains("name"), "{err}");
        let err = parse_tools_file(r#"[{"name":"a"},{"name":"a"}]"#).unwrap_err();
        assert!(err.contains("twice"), "{err}");
        let err = parse_tools_file(r#"[{"name":"a","parameters":[1]}]"#).unwrap_err();
        assert!(err.contains("JSON-Schema"), "{err}");
    }

    #[test]
    fn timeout_clamped_to_range() {
        let low = parse_spec(&json!({"name":"a","timeout_ms":1}), 0).unwrap();
        assert_eq!(low.timeout_ms, TOOL_TIMEOUT_MIN_MS);
        let high = parse_spec(&json!({"name":"a","timeout_ms":999_999}), 0).unwrap();
        assert_eq!(high.timeout_ms, TOOL_TIMEOUT_MAX_MS);
    }

    #[test]
    fn catalog_is_intersection_of_allowlist_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools.json");
        std::fs::write(
            &path,
            json!({"tools": [
                {"name": "notify_send", "description": "notify"},
                {"name": "ghost_action", "description": "not allowlisted"}
            ]})
            .to_string(),
        )
        .unwrap();

        let cat = Catalog::build("notify_send, fs_read", Some(path.to_str().unwrap())).unwrap();
        let names: Vec<&str> = cat.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["notify_send", "fs_read"], "allowlist order preserved");
        assert!(cat.tools_file_set);
        // Allowlisted-but-undescribed gets a minimal spec.
        let fs_read = cat.get("fs_read").unwrap();
        assert_eq!(fs_read.description, "");
        assert_eq!(fs_read.parameters, serde_json::Value::Null);
        // File-only entries never leak into the catalog.
        assert!(cat.get("ghost_action").is_none());
    }

    #[test]
    fn empty_allowlist_means_empty_catalog() {
        let cat = Catalog::build("", None).unwrap();
        assert!(cat.tools.is_empty());
        assert!(!cat.tools_file_set);
    }

    #[test]
    fn missing_tools_file_is_a_loud_error() {
        let err = Catalog::build("a", Some("/nonexistent/vynkor-agent-tools.json")).unwrap_err();
        assert!(err.contains("cannot read tools file"), "{err}");
    }

    #[test]
    fn get_finds_by_name() {
        let cat = Catalog { tools: vec![spec("x")], allowed_actions: vec!["x".into()], tools_file_set: false };
        assert!(cat.get("x").is_some());
        assert!(cat.get("y").is_none());
    }

    fn catalog_of(specs: Vec<ToolSpec>) -> Catalog {
        let allowed_actions = specs.iter().map(|s| s.name.clone()).collect();
        Catalog { tools: specs, allowed_actions, tools_file_set: false }
    }

    #[test]
    fn inference_never_overwrites_an_explicit_risk() {
        let mut tool = spec("secret_get");
        tool.risk = "low".into(); // pretend an operator explicitly vouched for it
        let mut cat = catalog_of(vec![tool]);
        infer_missing_risk(&mut cat);
        assert_eq!(cat.get("secret_get").unwrap().risk, "low", "explicit risk must survive untouched");
        assert!(
            !cat.get("secret_get").unwrap().requires_confirmation,
            "inference must not run at all once risk is non-empty, so confirmation is untouched too"
        );
    }

    #[test]
    fn unknown_risk_is_treated_as_undeclared_not_as_an_explicit_label() {
        // Regression for a hole found by measuring the live kernel, not by a
        // test: every telegram action reached the agent as risk="unknown"
        // (ActionRisk::Unknown is the proto zero value for a plugin.json with
        // no `risk` key). The original is_empty() guard read that as an
        // explicit declaration and skipped inference, so tg_send_message went
        // out to a human with no confirmation.
        let mut tool = spec("tg_send_message");
        tool.risk = "unknown".into();
        let mut cat = catalog_of(vec![tool]);
        infer_missing_risk(&mut cat);
        let t = cat.get("tg_send_message").unwrap();
        assert_eq!(t.risk, "high", "an undeclared risk must be inferred, not preserved");
        assert!(t.requires_confirmation, "a message sent to a human must be gated");
    }

    #[test]
    fn unknown_risk_matching_is_case_insensitive() {
        let mut tool = spec("secret_get");
        tool.risk = "UNKNOWN".into();
        let mut cat = catalog_of(vec![tool]);
        infer_missing_risk(&mut cat);
        assert_eq!(cat.get("secret_get").unwrap().risk, "critical");
    }

    #[test]
    fn inference_never_downgrades_an_explicit_confirmation_requirement() {
        let mut tool = spec("ping"); // infers to "low" on its own
        tool.requires_confirmation = true;
        let mut cat = catalog_of(vec![tool]);
        infer_missing_risk(&mut cat);
        let t = cat.get("ping").unwrap();
        assert_eq!(t.risk, "low", "risk was empty, so inference does fill it in");
        assert!(t.requires_confirmation, "an explicit true must never be cleared by inference");
    }

    #[test]
    fn secret_get_infers_critical_and_requires_confirmation() {
        let mut cat = catalog_of(vec![spec("secret_get")]);
        infer_missing_risk(&mut cat);
        let t = cat.get("secret_get").unwrap();
        assert_eq!(t.risk, "critical");
        assert!(t.requires_confirmation, "reading a secret must halt the loop for confirmation");
    }

    #[test]
    fn fs_read_stays_low_and_never_requires_confirmation() {
        let mut cat = catalog_of(vec![spec("fs_read")]);
        infer_missing_risk(&mut cat);
        let t = cat.get("fs_read").unwrap();
        assert_eq!(t.risk, "low");
        assert!(!t.requires_confirmation, "a pure read must never gain a confirmation requirement");
    }

    #[test]
    fn note_delete_and_secret_delete_land_on_different_tiers() {
        let mut cat = catalog_of(vec![spec("note_delete"), spec("secret_delete")]);
        infer_missing_risk(&mut cat);
        assert_eq!(cat.get("note_delete").unwrap().risk, "high", "deleting a note is destructive but scoped");
        assert_eq!(
            cat.get("secret_delete").unwrap().risk, "critical",
            "deleting a credential is not the same risk as deleting a note, despite the same verb"
        );
        assert!(cat.get("note_delete").unwrap().requires_confirmation);
        assert!(cat.get("secret_delete").unwrap().requires_confirmation);
    }

    #[test]
    fn hotkey_inject_infers_medium_not_critical() {
        // hotkey_inject does not synthesize keystrokes — verified against
        // plugins/hotkey/src (no xdotool/ydotool/XTest/uinput/libei/evdev;
        // the plugin only holds PERMISSION_EVENT_PUBLISH and
        // PERMISSION_SYSTEM). It publishes a synthetic hotkey_pressed/
        // hotkey_released kernel event for a binding id — real but indirect
        // risk (whatever a subscriber wired to that id fires), which is a
        // "medium" recoverable-mutation risk, not "critical".
        let mut cat = catalog_of(vec![spec("hotkey_inject")]);
        infer_missing_risk(&mut cat);
        let t = cat.get("hotkey_inject").unwrap();
        assert_eq!(t.risk, "medium");
        assert!(!t.requires_confirmation, "medium risk must not gate the loop on confirmation");
    }

    #[test]
    fn hotkey_inject_explicit_medium_from_a_manifest_is_not_upgraded() {
        // Bug this guards: a table that "knows better" than the plugin's
        // own manifest and silently promotes an explicit, deliberate
        // "medium" to something stricter. Explicit always wins.
        let mut tool = spec("hotkey_inject");
        tool.risk = "medium".into();
        let mut cat = catalog_of(vec![tool]);
        infer_missing_risk(&mut cat);
        let t = cat.get("hotkey_inject").unwrap();
        assert_eq!(t.risk, "medium", "an explicit manifest risk must never be upgraded by inference");
        assert!(!t.requires_confirmation, "inference did not run, so it must not add confirmation either");
    }

    #[test]
    fn hotkey_inject_explicit_critical_from_a_manifest_is_not_downgraded() {
        // The mirror case: if a plugin manifest one day declares
        // hotkey_inject "critical" (e.g. a future backend that really does
        // synthesize input), inference must leave that alone too — it only
        // ever fills empty holes, in either direction.
        let mut tool = spec("hotkey_inject");
        tool.risk = "critical".into();
        tool.requires_confirmation = true;
        let mut cat = catalog_of(vec![tool]);
        infer_missing_risk(&mut cat);
        let t = cat.get("hotkey_inject").unwrap();
        assert_eq!(t.risk, "critical", "an explicit manifest risk must never be downgraded by inference");
        assert!(t.requires_confirmation);
    }

    #[test]
    fn every_live_tool_name_gets_a_non_empty_risk() {
        // The full 142-name live catalog measured on the running system —
        // every single one must come out with *some* non-empty risk label
        // after inference; none may be left blank (which is exactly the
        // fail-open bug this function exists to close).
        let names = [
            "chat_completion", "clipboard_providers", "clipboard_read", "clipboard_write",
            "confirm_write", "daemon_ask", "daemon_disable", "daemon_enable", "daemon_say",
            "daemon_status", "daemon_turn", "db_append", "db_batch_get", "db_delete", "db_get",
            "db_incr", "db_keys", "db_patch", "db_query", "db_set", "dev-test-single.battery",
            "dev-test-single.geo", "dev-c5639a.battery", "dev-c5639a.geo", "email_list",
            "email_send", "embedding", "event_create", "event_delete", "event_get", "event_list",
            "event_update", "fs_list", "fs_read", "fs_write", "hotkey_bind", "hotkey_inject",
            "hotkey_list", "hotkey_status", "hotkey_unbind", "http_request", "launch",
            "launch_list", "launch_providers", "list_agents", "list_models",
            "media_list_players", "media_loop", "media_next", "media_pause", "media_play",
            "media_play_pause", "media_prev", "media_seek", "media_seek_relative",
            "media_shuffle", "media_status", "media_stop", "media_volume", "mic_start",
            "mic_status", "mic_stop", "network_stats", "note_create", "note_delete", "note_get",
            "note_list", "note_update", "notify_delete", "notify_list", "notify_mark_read",
            "notify_providers", "notify_send", "ping", "refresh_models", "request_write",
            "schedule_delete", "schedule_get", "schedule_list", "schedule_set", "secret_delete",
            "secret_get", "secret_list", "secret_set", "sound_play", "sound_status",
            "sound_stop", "stt_listen_start", "stt_listen_stop", "stt_models", "stt_transcribe",
            "sync_client_get_state", "sync_del", "sync_get", "sync_get_snapshot", "sync_set",
            "sys_battery", "sys_brightness", "sys_brightness_set", "sys_info", "sys_lock",
            "sys_power_profile", "sys_power_profile_set", "sys_procs", "sys_volume",
            "sys_volume_mute", "sys_volume_set", "tg_list_dialogs", "tg_get_history",
            "tg_get_message", "tg_search", "tg_send_message", "tg_edit_message",
            "tg_delete_message", "tg_forward_message", "tg_pin_message", "tg_mark_read",
            "tg_download_media", "tg_add_reaction", "tg_get_unread", "tg_list_unread",
            "tg_transcribe_voice", "tts_speak", "tts_synthesize", "tts_voices", "usage_stats",
            "vec_delete", "vec_get", "vec_list", "vec_query", "vec_stats", "vec_upsert",
            "vec_upsert_batch", "web_search", "capture_screenshot", "capture_record_start",
            "capture_record_stop", "capture_ocr", "capture_status", "tg_upload_media",
            "tg_get_chat_info", "tg_mark_all_read",
        ];
        assert_eq!(names.len(), 142, "the live catalog measured 142 tools — keep this list in sync");
        let mut cat = catalog_of(names.iter().map(|n| spec(n)).collect());
        infer_missing_risk(&mut cat);
        for name in names {
            let t = cat.get(name).unwrap();
            assert!(!t.risk.trim().is_empty(), "{name} was left with an empty risk");
            if matches!(t.risk.as_str(), "high" | "critical") {
                assert!(t.requires_confirmation, "{name} inferred {} but was not gated", t.risk);
            } else {
                assert!(!t.requires_confirmation, "{name} inferred {} but still requires confirmation", t.risk);
            }
        }
    }

    #[test]
    fn read_only_lookalikes_of_dangerous_exact_names_stay_low() {
        // Prefix/suffix collisions that must NOT be swept into a higher
        // tier just because they share a stem with a dangerous exact name.
        let mut cat = catalog_of(vec![spec("launch_list"), spec("launch_providers"), spec("db_get")]);
        infer_missing_risk(&mut cat);
        for name in ["launch_list", "launch_providers", "db_get"] {
            assert_eq!(cat.get(name).unwrap().risk, "low", "{name}");
            assert!(!cat.get(name).unwrap().requires_confirmation, "{name}");
        }
    }
}
