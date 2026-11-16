//! Token watchdog: rolling averages of prompt tokens per goal step and of
//! LLM steps per goal, and an alert when either climbs past a threshold.
//!
//! A broken embedding filter once pushed every step from ~5k to ~22k prompt
//! tokens and nobody noticed for a month. Normal steps sit around 4-5k; a
//! regression of that kind shows up within a few goals. A model guessing
//! tool names keeps its steps normal-sized but takes 5-7 of them where 2-3
//! do, so step counts are watched too.

use std::collections::VecDeque;
use std::sync::Mutex;

pub const TOKEN_ALERT_ENV: &str = "AGENT_PLUGIN_TOKEN_ALERT";
pub const DEFAULT_THRESHOLD: u64 = 12_000;
/// Steps averaged; a single huge step (a long tool result) is not a trend.
pub const WINDOW: usize = 8;
pub const COOLDOWN_MS: i64 = 6 * 3_600_000;
/// Average LLM steps per goal over the last `WINDOW` goals; normal is 2-3.
pub const STEPS_THRESHOLD: u64 = 4;

/// `None` = disabled (`off`/`0`). Garbage falls back to the default rather
/// than silently disabling the alarm.
pub fn threshold() -> Option<u64> {
    parse_threshold(std::env::var(TOKEN_ALERT_ENV).ok().as_deref())
}

fn parse_threshold(raw: Option<&str>) -> Option<u64> {
    match raw.map(str::trim) {
        Some("off" | "0") => None,
        Some(s) => Some(s.parse().unwrap_or(DEFAULT_THRESHOLD)),
        None => Some(DEFAULT_THRESHOLD),
    }
}

#[derive(Debug, Default)]
pub struct Watchdog {
    recent: VecDeque<u64>,
    last_alert_ms: Option<i64>,
}

impl Watchdog {
    /// Records one value; returns the window's average when the full
    /// window's average exceeds `threshold` and the last alert is older than
    /// the cooldown. The window restarts after an alert so one spike period
    /// raises one alert. Zero is ignored (provider reported no usage).
    pub fn observe(&mut self, value: u64, threshold: u64, now_ms: i64) -> Option<u64> {
        if value == 0 {
            return None;
        }
        self.recent.push_back(value);
        if self.recent.len() > WINDOW {
            self.recent.pop_front();
        }
        if self.recent.len() < WINDOW {
            return None;
        }
        let avg = self.recent.iter().sum::<u64>() / WINDOW as u64;
        let cooled = self.last_alert_ms.is_none_or(|t| now_ms - t >= COOLDOWN_MS);
        if avg <= threshold || !cooled {
            return None;
        }
        self.last_alert_ms = Some(now_ms);
        self.recent.clear();
        Some(avg)
    }
}

static TOKENS: Mutex<Option<Watchdog>> = Mutex::new(None);
static STEPS: Mutex<Option<Watchdog>> = Mutex::new(None);

fn observe_in(slot: &Mutex<Option<Watchdog>>, value: u64, threshold: u64, now_ms: i64) -> Option<u64> {
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    guard.get_or_insert_with(Watchdog::default).observe(value, threshold, now_ms)
}

/// One goal step's prompt tokens; the alert text, `None` while disabled.
pub fn observe(input_tokens: u64, now_ms: i64) -> Option<String> {
    let threshold = threshold()?;
    let avg = observe_in(&TOKENS, input_tokens, threshold, now_ms)?;
    Some(format!(
        "Агент: средний промпт за последние {WINDOW} шагов — {avg} токенов (порог {threshold}). \
         Обычно ~5k: проверь фильтр инструментов (лог агента: \"embedding filter\")."
    ))
}

/// One finished goal's LLM step count; same switch as the token alert.
pub fn observe_goal(llm_steps: u64, now_ms: i64) -> Option<String> {
    threshold()?;
    let avg = observe_in(&STEPS, llm_steps, STEPS_THRESHOLD, now_ms)?;
    Some(format!(
        "Агент: в среднем {avg} шагов LLM на задачу за последние {WINDOW} задач (обычно 2-3). \
         Похоже, модель перебирает инструменты: проверь шаги unknown_tool в goal_list."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alerts_once_on_a_sustained_climb_then_cools_down() {
        let mut w = Watchdog::default();
        for _ in 0..20 {
            assert!(w.observe(4_500, DEFAULT_THRESHOLD, 0).is_none(), "normal steps never alert");
        }
        let mut alerts = 0;
        for i in 0..WINDOW as i64 {
            alerts += w.observe(22_000, DEFAULT_THRESHOLD, i).is_some() as u32;
        }
        assert_eq!(alerts, 1, "the climb alerts once the window is mostly high");
        for i in 0..(3 * WINDOW as i64) {
            assert!(w.observe(22_000, DEFAULT_THRESHOLD, 100 + i).is_none(), "cooldown holds");
        }
        let later = COOLDOWN_MS + 1_000;
        let again = (0..WINDOW as i64).filter_map(|i| w.observe(22_000, DEFAULT_THRESHOLD, later + i)).count();
        assert_eq!(again, 1, "after the cooldown a still-high average alerts again");
    }

    #[test]
    fn one_spike_is_not_a_trend_and_zero_usage_is_ignored() {
        let mut w = Watchdog::default();
        for _ in 0..WINDOW - 1 {
            w.observe(4_000, DEFAULT_THRESHOLD, 0);
        }
        assert!(w.observe(60_000, DEFAULT_THRESHOLD, 0).is_none(), "avg 11k stays under 12k");
        assert!(w.observe(0, DEFAULT_THRESHOLD, 0).is_none());
    }

    #[test]
    fn step_counts_alert_on_a_guessing_streak() {
        let mut w = Watchdog::default();
        for _ in 0..WINDOW {
            assert!(w.observe(2, STEPS_THRESHOLD, 0).is_none());
        }
        let alerts = (0..WINDOW as i64).filter_map(|i| w.observe(7, STEPS_THRESHOLD, i)).count();
        assert_eq!(alerts, 1);
    }

    #[test]
    fn threshold_env_parsing() {
        assert_eq!(parse_threshold(None), Some(DEFAULT_THRESHOLD));
        assert_eq!(parse_threshold(Some("8000")), Some(8_000));
        assert_eq!(parse_threshold(Some("off")), None);
        assert_eq!(parse_threshold(Some("0")), None);
        assert_eq!(parse_threshold(Some("lots")), Some(DEFAULT_THRESHOLD));
    }
}
