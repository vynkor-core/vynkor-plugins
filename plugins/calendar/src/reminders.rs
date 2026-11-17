//! Pure reminder-selection logic for the `calendar` plugin.
//!
//! An event participates in reminders only when `remind_before_ms` is set;
//! a reminder fires once (`reminder_fired` guards re-firing) at
//! `start_ms - remind_before_ms`. Selection is a pure function so the scan
//! loop stays trivial and testable.

use crate::store::EventDoc;

/// One reminder due for firing: the event snapshot plus derived fields.
#[derive(Debug, Clone, PartialEq)]
pub struct DueFire {
    pub event: EventDoc,
    pub remind_at_ms: i64,
    /// True when the reminder's time already passed the event start — the
    /// plugin was down past start_ms, or the scan interval overshot.
    pub late: bool,
}

/// Select every event whose reminder is due at `now_ms` and not yet fired.
/// Order follows the input slice order; the caller sorts if it cares.
pub fn due_events(events: &[EventDoc], now_ms: i64) -> Vec<DueFire> {
    events
        .iter()
        .filter_map(|e| {
            let lead = e.remind_before_ms?;
            if e.reminder_fired {
                return None;
            }
            let remind_at = e.start_ms.saturating_sub(lead);
            if remind_at > now_ms {
                return None;
            }
            Some(DueFire {
                event: e.clone(),
                remind_at_ms: remind_at,
                late: now_ms > e.start_ms,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, start_ms: i64, remind_before_ms: Option<i64>, fired: bool) -> EventDoc {
        EventDoc {
            id: id.to_string(),
            title: format!("event-{id}"),
            description: String::new(),
            start_ms,
            end_ms: None,
            all_day: false,
            remind_before_ms,
            reminder_fired: fired,
            tags: vec![],
            ics_uid: None,
            rrule: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn skips_events_without_reminder_fired_or_future() {
        let events = [
            event("1", 100, None, false),         // no reminder configured
            event("2", 100, Some(50), true),      // already fired
            event("3", 10_000, Some(500), false), // remind_at 9_500 > now 1_000
        ];
        assert!(due_events(&events, 1_000).is_empty());
    }

    #[test]
    fn fires_due_reminders_with_late_flag() {
        let events = [
            // remind_at exactly now → due, not late (start in the future).
            event("1", 5_000, Some(3_000), false),
            // remind_at passed AND start passed → due and late.
            event("2", 900, Some(100), false),
        ];
        let fires = due_events(&events, 2_000);
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].remind_at_ms, 2_000);
        assert!(!fires[0].late);
        assert_eq!(fires[1].remind_at_ms, 800);
        assert!(fires[1].late);
    }

    #[test]
    fn zero_lead_fires_exactly_at_start() {
        let events = [event("1", 7_000, Some(0), false)];
        assert!(due_events(&events, 6_999).is_empty());
        let fires = due_events(&events, 7_000);
        assert_eq!(fires.len(), 1);
        assert!(!fires[0].late);
    }

    #[test]
    fn saturates_instead_of_overflowing_on_huge_leads() {
        // A lead of i64::MAX can't come through request parsing (capped at
        // MAX_REMIND_BEFORE_MS), but the pure function must stay panic-free
        // on hostile stored docs anyway: the reminder simply lands far in
        // the past and fires late.
        let e = event("1", 1_000, Some(i64::MAX), false);
        let fires = due_events(&[e], 5_000);
        assert_eq!(fires.len(), 1);
        assert!(fires[0].remind_at_ms < 5_000);
        assert!(fires[0].late);
    }
}
