//! Threshold alerts over samples (`METRICS_PLUGIN_ALERTS`).
//!
//! A rule is `<metric><op><limit>`, e.g. `battery_percent<15` or
//! `disk_used_percent>90`. Each rule publishes `threshold` once when a sample
//! crosses into breach and `threshold_cleared` once it is back past the limit
//! by the hysteresis margin — never one event per sample.

use serde_json::{json, Value};

/// Numeric sample fields a rule may name.
pub const METRICS: [&str; 4] = ["cpu_load_1", "mem_used_percent", "disk_used_percent", "battery_percent"];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op { Lt, Gt }

#[derive(Debug, Clone, PartialEq)]
pub struct Rule { pub metric: String, pub op: Op, pub limit: f64 }

impl Rule {
    fn breached(&self, v: f64) -> bool {
        match self.op { Op::Lt => v < self.limit, Op::Gt => v > self.limit }
    }
    /// True once `v` is back on the healthy side by at least `margin`.
    fn cleared(&self, v: f64, margin: f64) -> bool {
        match self.op { Op::Lt => v >= self.limit + margin, Op::Gt => v <= self.limit - margin }
    }
    fn op_str(&self) -> &'static str { match self.op { Op::Lt => "<", Op::Gt => ">" } }
}

/// Parses a comma-separated rule list. Bad entries are returned as errors
/// (for the log) and skipped; the good ones still apply.
pub fn parse_rules(spec: &str) -> (Vec<Rule>, Vec<String>) {
    let mut rules = Vec::new();
    let mut errors = Vec::new();
    for raw in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some(pos) = raw.find(['<', '>']) else {
            errors.push(format!("{raw:?}: expected <metric><op><limit> with op < or >"));
            continue;
        };
        let metric = raw[..pos].trim();
        let op = if raw.as_bytes()[pos] == b'<' { Op::Lt } else { Op::Gt };
        if !METRICS.contains(&metric) {
            errors.push(format!("{raw:?}: unknown metric {metric:?} (one of {})", METRICS.join(", ")));
            continue;
        }
        match raw[pos + 1..].trim().parse::<f64>() {
            Ok(limit) if limit.is_finite() => rules.push(Rule { metric: metric.to_string(), op, limit }),
            _ => errors.push(format!("{raw:?}: limit is not a number")),
        }
    }
    (rules, errors)
}

pub struct Alerts { rules: Vec<Rule>, active: Vec<bool>, hysteresis: f64 }

impl Alerts {
    pub fn new(rules: Vec<Rule>, hysteresis: f64) -> Self {
        let active = vec![false; rules.len()];
        Self { rules, active, hysteresis: hysteresis.max(0.0) }
    }

    pub fn is_empty(&self) -> bool { self.rules.is_empty() }

    /// Feeds one sample (as serialized JSON) through every rule and returns
    /// the events to publish. A metric the host doesn't report (no battery)
    /// leaves its rule untouched. A battery that is charging never counts as
    /// low, so plugging in clears a low-battery alert.
    pub fn evaluate(&mut self, sample: &Value) -> Vec<(String, Value)> {
        let charging = sample.get("battery_charging").and_then(Value::as_bool) == Some(true);
        let mut events = Vec::new();
        for (rule, active) in self.rules.iter().zip(self.active.iter_mut()) {
            let Some(value) = sample.get(&rule.metric).and_then(Value::as_f64) else { continue };
            let charging_battery = charging && rule.metric == "battery_percent";
            let payload = json!({"metric": rule.metric, "op": rule.op_str(), "limit": rule.limit, "value": value});
            if !*active && rule.breached(value) && !charging_battery {
                *active = true;
                events.push(("threshold".to_string(), payload));
            } else if *active && (rule.cleared(value, self.hysteresis) || charging_battery) {
                *active = false;
                events.push(("threshold_cleared".to_string(), payload));
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules_and_reports_bad_ones() {
        let (rules, errors) = parse_rules(" battery_percent<15, disk_used_percent > 90 ,,cpu_temp>80,mem_used_percent=5,disk_used_percent>x");
        assert_eq!(rules, vec![
            Rule { metric: "battery_percent".into(), op: Op::Lt, limit: 15.0 },
            Rule { metric: "disk_used_percent".into(), op: Op::Gt, limit: 90.0 },
        ]);
        assert_eq!(errors.len(), 3, "{errors:?}");
    }

    fn disk(v: f64) -> Value { json!({"disk_used_percent": v}) }

    #[test]
    fn fires_once_per_crossing_with_hysteresis() {
        let (rules, _) = parse_rules("disk_used_percent>90");
        let mut a = Alerts::new(rules, 2.0);
        assert!(a.evaluate(&disk(89.0)).is_empty());
        let ev = a.evaluate(&disk(91.0));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, "threshold");
        assert_eq!(ev[0].1["value"], 91.0);
        assert!(a.evaluate(&disk(95.0)).is_empty(), "still breached: no repeat");
        assert!(a.evaluate(&disk(89.0)).is_empty(), "inside the hysteresis band");
        assert!(a.evaluate(&disk(90.5)).is_empty(), "wobbling around the limit stays quiet");
        let ev = a.evaluate(&disk(88.0));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, "threshold_cleared");
        assert_eq!(a.evaluate(&disk(92.0))[0].0, "threshold", "a new crossing fires again");
    }

    #[test]
    fn missing_metric_is_ignored_and_charging_clears_low_battery() {
        let (rules, _) = parse_rules("battery_percent<15");
        let mut a = Alerts::new(rules, 2.0);
        assert!(a.evaluate(&json!({"battery_percent": null})).is_empty(), "desktop without battery");
        assert!(a.evaluate(&json!({"battery_percent": 10, "battery_charging": true})).is_empty(), "charging is not low");
        assert_eq!(a.evaluate(&json!({"battery_percent": 10, "battery_charging": false}))[0].0, "threshold");
        assert_eq!(a.evaluate(&json!({"battery_percent": 11, "battery_charging": true}))[0].0, "threshold_cleared");
    }
}
