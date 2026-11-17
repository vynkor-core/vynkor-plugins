//! Learned routines: repeated single-tool device commands the agent mined
//! from its own goal history (overnight, on a local model) and now serves
//! with no LLM call — dispatch the known tool call, answer with one of the
//! replies generated at mining time.
//!
//! Spec: `docs/superpowers/specs/2026-10-26-learned-routines-design.md`.
//!
//! - [`fire`] — the goal-start hook (match, dispatch, self-disable);
//! - [`guard`] — deterministic checks on what the mining model writes;
//! - [`mine`] — candidate extraction + one local-model call per candidate;
//! - [`store`] — the whole set persisted as one `database` doc.
//!
//! Only exact matches on the normalized text fire (see
//! [`crate::smalltalk::normalize`]): "свет на кухне" and "свет в спальне"
//! embed nearly the same but must not share a routine.

pub mod fire;
pub mod guard;
pub mod mine;
pub mod store;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::ToolSpec;

pub const ROUTINES_ENV: &str = "AGENT_PLUGIN_ROUTINES";
pub const ROUTINE_TOOLS_ENV: &str = "AGENT_PLUGIN_ROUTINE_TOOLS";
pub const ROUTINES_AGENT_ENV: &str = "AGENT_PLUGIN_ROUTINES_AGENT_ID";
pub const ROUTINES_AT_ENV: &str = "AGENT_PLUGIN_ROUTINES_AT";
pub const LOOKBACK_DAYS_ENV: &str = "AGENT_PLUGIN_ROUTINES_LOOKBACK_DAYS";

/// An allowlist, not a denylist: the live catalog carries message-sending
/// tools with no confirmation flag, and those must never be learned.
pub const DEFAULT_ROUTINE_TOOLS: &str = "mqtt_device_command,pixel-6pro.flashlight,\
pixel-6pro.dnd,pixel-6pro.ringer,pixel-6pro.brightness";
pub const DEFAULT_AT: (u32, u32) = (3, 30);
pub const DEFAULT_LOOKBACK_DAYS: i64 = 30;
/// How long after a routine fired a correction ("не тот") still blames it.
pub const CORRECTION_WINDOW_MS: i64 = 120_000;
const CORRECTION_MARKERS: &[&str] = &[
    "не то",
    "не тот",
    "не так",
    "неправильно",
    "отмена",
    "отмени",
    "верни",
];

pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_DISABLED: &str = "disabled";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Routine {
    pub id: String,
    pub tool: String,
    pub params: Value,
    /// Normalized phrases that fire this routine.
    #[serde(default)]
    pub phrases: Vec<String>,
    #[serde(default)]
    pub replies: Vec<String>,
    /// [`spec_hash`] of the tool when learned; a mismatch at fire time
    /// means the tool changed under the routine.
    #[serde(default)]
    pub spec_hash: String,
    #[serde(default = "default_status")]
    pub status: String,
    /// `tool_changed` | `tool_error` | `user_correction`
    #[serde(default)]
    pub disabled_reason: String,
    #[serde(default)]
    pub disabled_at_ms: i64,
    #[serde(default)]
    pub hits: u32,
    #[serde(default)]
    pub fails: u32,
    #[serde(default)]
    pub last_reply_idx: usize,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub updated_at_ms: i64,
}

fn default_status() -> String {
    STATUS_ACTIVE.to_string()
}

impl Routine {
    pub fn is_active(&self) -> bool {
        self.status == STATUS_ACTIVE
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastFire {
    pub routine_id: String,
    pub at_ms: i64,
}

/// Every routine plus the last fire (for correction blame).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Book {
    #[serde(default)]
    pub routines: Vec<Routine>,
    #[serde(default)]
    pub last_fire: Option<LastFire>,
}

impl Book {
    pub fn find_active(&self, normalized: &str) -> Option<usize> {
        if normalized.is_empty() {
            return None;
        }
        self.routines
            .iter()
            .position(|r| r.is_active() && r.phrases.iter().any(|p| p == normalized))
    }

    /// Next reply in rotation — consecutive fires never repeat a reply
    /// while there are at least two.
    pub fn next_reply(&mut self, idx: usize) -> String {
        let r = &mut self.routines[idx];
        if r.replies.is_empty() {
            return String::new();
        }
        r.last_reply_idx = (r.last_reply_idx + 1) % r.replies.len();
        r.replies[r.last_reply_idx].clone()
    }

    /// The routine a correction arriving at `now_ms` blames, if any.
    pub fn correction_target(&self, now_ms: i64) -> Option<usize> {
        let last = self.last_fire.as_ref()?;
        if now_ms - last.at_ms > CORRECTION_WINDOW_MS {
            return None;
        }
        self.routines
            .iter()
            .position(|r| r.id == last.routine_id && r.is_active())
    }

    pub fn disable(&mut self, idx: usize, reason: &str, now_ms: i64) {
        let r = &mut self.routines[idx];
        r.status = STATUS_DISABLED.to_string();
        r.disabled_reason = reason.to_string();
        r.disabled_at_ms = now_ms;
        r.updated_at_ms = now_ms;
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn enabled() -> bool {
    matches!(
        env_nonempty(ROUTINES_ENV)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "on" | "true" | "1" | "yes"
    )
}

pub fn allowed_tools() -> Vec<String> {
    env_nonempty(ROUTINE_TOOLS_ENV)
        .unwrap_or_else(|| DEFAULT_ROUTINE_TOOLS.to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The `ai` profile mining runs on. `None` refuses mining: falling back to
/// the goal's (cloud) plan would spend the tokens this feature saves.
pub fn mining_agent_id() -> Option<String> {
    env_nonempty(ROUTINES_AGENT_ENV).or_else(|| env_nonempty(crate::memory::MEMORY_AGENT_ENV))
}

pub fn lookback_days() -> i64 {
    env_nonempty(LOOKBACK_DAYS_ENV)
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_LOOKBACK_DAYS)
}

pub fn eligible(spec: &ToolSpec, allow: &[String]) -> bool {
    allow.iter().any(|a| a == &spec.name)
        && !spec.requires_confirmation
        && !spec.risk.eq_ignore_ascii_case("high")
}

/// FNV-1a 64 over name, description and parameter schema — stable across
/// toolchains, unlike `DefaultHasher`, so a rebuild never disables routines.
pub fn spec_hash(spec: &ToolSpec) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let text = format!("{}\0{}\0{}", spec.name, spec.description, spec.parameters);
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn is_correction(normalized: &str) -> bool {
    CORRECTION_MARKERS.iter().any(|m| {
        normalized == *m
            || normalized
                .strip_prefix(m)
                .is_some_and(|rest| rest.starts_with(' '))
    })
}

pub fn parse_at(raw: &str) -> Option<(u32, u32)> {
    let (h, m) = raw.trim().split_once(':')?;
    let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

pub fn mining_at() -> (u32, u32) {
    env_nonempty(ROUTINES_AT_ENV)
        .and_then(|s| parse_at(&s))
        .unwrap_or(DEFAULT_AT)
}

/// Time from `now` until the next local `at` (tomorrow if already past).
pub fn next_run_delay<Tz: chrono::TimeZone>(now: chrono::DateTime<Tz>, at: (u32, u32)) -> Duration {
    use chrono::Timelike;
    let now_s = now.num_seconds_from_midnight() as i64;
    let at_s = (at.0 * 3600 + at.1 * 60) as i64;
    let mut delta = at_s - now_s;
    if delta <= 0 {
        delta += 86_400;
    }
    Duration::from_secs(delta as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smalltalk::normalize;
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: "turn things on".to_string(),
            parameters: json!({"type": "object"}),
            requires_confirmation: false,
            risk: "low".to_string(),
            timeout_ms: 1000,
            cooldown_ms: 0,
            max_per_goal: 16,
            source: crate::tools::Source::Minimal,
        }
    }

    fn routine(id: &str, phrase: &str, replies: &[&str]) -> Routine {
        Routine {
            id: id.to_string(),
            tool: "mqtt_device_command".to_string(),
            params: json!({"pin": "D1"}),
            phrases: vec![phrase.to_string()],
            replies: replies.iter().map(|s| s.to_string()).collect(),
            spec_hash: String::new(),
            status: STATUS_ACTIVE.to_string(),
            disabled_reason: String::new(),
            disabled_at_ms: 0,
            hits: 0,
            fails: 0,
            last_reply_idx: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn normalize_is_case_punctuation_and_yo_insensitive() {
        assert_eq!(normalize("Включи  СВЕТ!!"), "включи свет");
        assert_eq!(normalize("ёлка"), "елка");
        assert_eq!(normalize("👍"), "");
    }

    #[test]
    fn eligibility_needs_allowlist_no_confirmation_and_not_high_risk() {
        let allow = vec!["mqtt_device_command".to_string()];
        assert!(eligible(&spec("mqtt_device_command"), &allow));
        assert!(!eligible(&spec("tg_send_message"), &allow));
        let mut gated = spec("mqtt_device_command");
        gated.requires_confirmation = true;
        assert!(!eligible(&gated, &allow));
        let mut risky = spec("mqtt_device_command");
        risky.risk = "high".to_string();
        assert!(!eligible(&risky, &allow));
    }

    #[test]
    fn spec_hash_tracks_description_and_schema() {
        let a = spec("t");
        assert_eq!(spec_hash(&a), spec_hash(&spec("t")));
        let mut b = spec("t");
        b.description = "other".into();
        assert_ne!(spec_hash(&a), spec_hash(&b));
        let mut c = spec("t");
        c.parameters = json!({"type": "object", "required": ["pin"]});
        assert_ne!(spec_hash(&a), spec_hash(&c));
    }

    #[test]
    fn find_active_matches_exact_phrases_of_active_routines_only() {
        let mut book = Book {
            routines: vec![routine("r1", "включи свет", &["Готово."])],
            last_fire: None,
        };
        assert_eq!(book.find_active("включи свет"), Some(0));
        assert_eq!(book.find_active("включи свет на кухне"), None);
        assert_eq!(book.find_active(""), None);
        book.disable(0, "tool_error", 1);
        assert_eq!(book.find_active("включи свет"), None);
    }

    #[test]
    fn next_reply_rotates_without_consecutive_repeats() {
        let mut book = Book {
            routines: vec![routine("r1", "x", &["a", "b", "c"])],
            last_fire: None,
        };
        let got: Vec<String> = (0..4).map(|_| book.next_reply(0)).collect();
        assert!(got.windows(2).all(|w| w[0] != w[1]), "{got:?}");
        let mut single = Book {
            routines: vec![routine("r1", "x", &["only"])],
            last_fire: None,
        };
        assert_eq!(single.next_reply(0), "only");
        assert_eq!(single.next_reply(0), "only");
    }

    #[test]
    fn corrections_are_recognised_by_leading_marker() {
        assert!(is_correction("не тот свет"));
        assert!(is_correction("отмена"));
        assert!(!is_correction("включи свет"));
        assert!(!is_correction("отменный день"));
    }

    #[test]
    fn correction_blames_the_last_fire_inside_the_window() {
        let mut book = Book {
            routines: vec![routine("r1", "x", &["a"])],
            last_fire: Some(LastFire {
                routine_id: "r1".into(),
                at_ms: 1_000,
            }),
        };
        assert_eq!(book.correction_target(1_000 + 119_000), Some(0));
        assert_eq!(book.correction_target(1_000 + 121_000), None);
        book.disable(0, "tool_error", 2_000);
        assert_eq!(book.correction_target(2_000), None);
    }

    #[test]
    fn mining_time_parses_and_schedules_next_occurrence() {
        assert_eq!(parse_at("03:30"), Some((3, 30)));
        assert_eq!(parse_at("25:00"), None);
        assert_eq!(parse_at("junk"), None);
        use chrono::TimeZone;
        let at_two = chrono::Utc.with_ymd_and_hms(2026, 10, 26, 2, 0, 0).unwrap();
        assert_eq!(
            next_run_delay(at_two, (3, 30)),
            Duration::from_secs(90 * 60)
        );
        let at_four = chrono::Utc.with_ymd_and_hms(2026, 10, 26, 4, 0, 0).unwrap();
        assert_eq!(
            next_run_delay(at_four, (3, 30)),
            Duration::from_secs((23 * 60 + 30) * 60)
        );
    }
}
