//! Minimal iCalendar (RFC 5545) subset for `calendar_ics_import` /
//! `calendar_ics_export`.
//!
//! Scope (v1, see PLANS.md EXI-01): VEVENT properties SUMMARY,
//! DESCRIPTION, LOCATION, UID, DTSTART, DTEND, RRULE (DAILY/WEEKLY/MONTHLY
//! with INTERVAL/COUNT/UNTIL/BYDAY-for-weekly) and VALARM TRIGGER as a
//! relative duration before the start. Line folding, text escaping and the
//! UTC/date value formats are handled here; TZID values are read as their
//! naive wall-clock time interpreted as UTC (a documented limitation — full
//! timezone support stays out until a real calendar source needs it).
//!
//! Date arithmetic is pure integer math over unix-milliseconds (civil-days
//! conversion, weekday), so every piece below is unit-testable offline.

/// Import window: occurrences outside `[now - 1d, now + horizon]` are dropped.
pub const IMPORT_HORIZON_DAYS: i64 = 90;
/// Hard cap on documents one import writes (protects `database` from a
/// pathological RRULE expansion).
pub const MAX_IMPORT_DOCS: usize = 500;

pub const MS_PER_DAY: i64 = 86_400_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    pub title: String,
    /// DESCRIPTION plus LOCATION folded in as a first line (`Location: …`).
    pub description: String,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    pub all_day: bool,
    pub remind_before_ms: Option<i64>,
    pub uid: String,
    /// Raw RRULE value; expanded by [`occurrences`] within the import window.
    pub rrule: Option<String>,
}

// ---------------------------------------------------------------------------
// civil-date math (Howard Hinnant's algorithms; Monday = weekday 0)
// ---------------------------------------------------------------------------

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn weekday_mon0(days: i64) -> u32 {
    (((days + 3) % 7) + 7) as u32 % 7
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 30,
    }
}

// ---------------------------------------------------------------------------
// timestamp <-> ICS value forms
// ---------------------------------------------------------------------------

/// `YYYYMMDD[T[H[HMM[SS]]]]Z` — returns `(unix_ms, is_date_only)`.
/// A missing trailing `Z` (floating local time) is read as UTC.
pub fn parse_dt(value: &str) -> Result<(i64, bool), String> {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return Err(format!("unparseable date value: {value:?}"));
    }
    let num = |from: usize, len: usize| -> Result<i64, String> {
        digits
            .get(from..from + len)
            .ok_or_else(|| format!("unparseable date value: {value:?}"))?
            .parse()
            .map_err(|_| format!("unparseable date value: {value:?}"))
    };
    let year = num(0, 4)?;
    let month = num(4, 2)?;
    let day = num(6, 2)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month as u32) as i64 {
        return Err(format!("date out of range: {value:?}"));
    }
    let date_only = !value.contains('T');
    if date_only {
        return Ok((
            days_from_civil(year, month as u32, day as u32) * MS_PER_DAY,
            true,
        ));
    }
    let hour = if digits.len() >= 10 { num(8, 2)? } else { 0 };
    let minute = if digits.len() >= 12 { num(10, 2)? } else { 0 };
    let second = if digits.len() >= 14 { num(12, 2)? } else { 0 };
    if hour > 23 || minute > 59 || second > 60 {
        return Err(format!("time out of range: {value:?}"));
    }
    let ms = days_from_civil(year, month as u32, day as u32) * MS_PER_DAY
        + hour * 3_600_000
        + minute * 60_000
        + second * 1_000;
    Ok((ms, false))
}

/// UTC `YYYYMMDDTHHMMSSZ`, or `YYYYMMDD` when `all_day`.
pub fn fmt_dt(ms: i64, all_day: bool) -> String {
    let days = ms.div_euclid(MS_PER_DAY);
    let rem = ms.rem_euclid(MS_PER_DAY);
    let (y, mo, d) = civil_from_days(days);
    if all_day {
        return format!("{y:04}{mo:02}{d:02}");
    }
    let h = rem / 3_600_000;
    let mi = (rem % 3_600_000) / 60_000;
    let s = (rem % 60_000) / 1_000;
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

// ---------------------------------------------------------------------------
// text escaping (RFC 5545 §3.3.11)
// ---------------------------------------------------------------------------

pub fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' | '\r' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

pub fn unescape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// line unfolding + property splitting
// ---------------------------------------------------------------------------

/// RFC 5545 folding: CRLF/LF followed by space or tab continues the line.
pub fn unfold_lines(input: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in input.lines() {
        if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
            let last = lines.last_mut().expect("checked non-empty");
            last.push_str(&raw[1..]);
        } else {
            lines.push(raw.to_string());
        }
    }
    lines
}

/// `NAME;PARAM=…;PARAM="quoted:value":value` → `(NAME, value)` — the colon
/// split is quote-aware so a quoted parameter containing `:` survives.
fn split_property(line: &str) -> Option<(String, String)> {
    let mut in_quotes = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                let name: String = line[..idx]
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_uppercase();
                return Some((name, line[idx + 1..].to_string()));
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// durations (VALARM TRIGGER)
// ---------------------------------------------------------------------------

/// `[+-]P[nD][T[nH][nM][nS]]` → milliseconds. `None` when the value is not a
/// relative duration (absolute-date triggers are ignored in v1).
pub fn parse_duration(value: &str) -> Option<i64> {
    let (sign, rest) = match value.strip_prefix('-') {
        Some(r) => (-1_i64, r),
        None => (1_i64, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P')?;
    let mut days = 0_i64;
    let mut hours = 0_i64;
    let mut minutes = 0_i64;
    let mut seconds = 0_i64;
    let mut number = String::new();
    let mut in_time = false;
    for ch in rest.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        let n: i64 = if number.is_empty() {
            0
        } else {
            number.parse().ok()?
        };
        number.clear();
        match ch {
            'D' if !in_time => days = n,
            'T' if !in_time => in_time = true,
            'H' if in_time => hours = n,
            'M' if in_time => minutes = n,
            'S' if in_time => seconds = n,
            'W' if !in_time => days = n * 7,
            _ => return None,
        }
    }
    Some(sign * (days * MS_PER_DAY + hours * 3_600_000 + minutes * 60_000 + seconds * 1_000))
}

/// Compact positive-duration form used in generated VALARMs.
fn fmt_duration_before(ms: i64) -> String {
    if ms % 3_600_000 == 0 {
        format!("-PT{}H", ms / 3_600_000)
    } else if ms % 60_000 == 0 {
        format!("-PT{}M", ms / 60_000)
    } else {
        format!("-PT{}S", ms / 1_000)
    }
}

// ---------------------------------------------------------------------------
// RRULE expansion
// ---------------------------------------------------------------------------

fn parse_byday(value: &str) -> Vec<u32> {
    const DAYS: [(&str, u32); 7] = [
        ("MO", 0),
        ("TU", 1),
        ("WE", 2),
        ("TH", 3),
        ("FR", 4),
        ("SA", 5),
        ("SU", 6),
    ];
    value
        .split(',')
        .filter_map(|token| {
            let token = token.trim();
            DAYS.iter()
                .find(|(name, _)| token.ends_with(name))
                .map(|(_, wd)| *wd)
        })
        .collect()
}

/// Expand an RRULE into occurrence start times strictly AFTER `first_ms`,
/// capped by `horizon_end_ms`, `count_remaining` (occurrences still allowed
/// AFTER the first — the caller owns the COUNT-includes-first arithmetic)
/// and a hard expansion cap so a pathological rule cannot explode.
pub fn expand_rrule(
    first_ms: i64,
    rrule: &str,
    horizon_end_ms: i64,
    count_remaining: Option<i64>,
    max_occurrences: usize,
) -> Vec<i64> {
    let mut freq = String::new();
    let mut interval = 1_i64;
    let mut count: Option<i64> = count_remaining;
    let mut until_ms: Option<i64> = None;
    let mut byday: Vec<u32> = Vec::new();
    for part in rrule.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => freq = value.trim().to_ascii_uppercase(),
            "INTERVAL" => interval = value.trim().parse().unwrap_or(1).max(1),
            "UNTIL" => until_ms = parse_dt(value.trim()).ok().map(|(ms, _)| ms),
            "BYDAY" => byday = parse_byday(value),
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut push = |ms: i64, out: &mut Vec<i64>| -> bool {
        if ms <= first_ms || ms > horizon_end_ms {
            return true;
        }
        if let Some(until) = until_ms {
            if ms > until {
                return false;
            }
        }
        if let Some(c) = count {
            if c <= 0 {
                return false;
            }
            count = Some(c - 1);
        }
        out.push(ms);
        out.len() < max_occurrences
    };

    let mut cursor_days = first_ms.div_euclid(MS_PER_DAY);
    match freq.as_str() {
        "DAILY" => {
            while cursor_days * MS_PER_DAY <= horizon_end_ms {
                let ms = cursor_days * MS_PER_DAY + first_ms.rem_euclid(MS_PER_DAY);
                if !push(ms, &mut out) {
                    break;
                }
                cursor_days += interval;
            }
        }
        "WEEKLY" if !byday.is_empty() => {
            // Weeks are Monday-anchored; the anchor week contains first_ms.
            let week_start = cursor_days - weekday_mon0(cursor_days) as i64;
            let mut week = week_start;
            'weeks: while week * MS_PER_DAY <= horizon_end_ms {
                for offset in 0..7_u32 {
                    let day = week + offset as i64;
                    if !byday.contains(&weekday_mon0(day)) {
                        continue;
                    }
                    let ms = day * MS_PER_DAY + first_ms.rem_euclid(MS_PER_DAY);
                    if !push(ms, &mut out) {
                        break 'weeks;
                    }
                }
                week += 7 * interval;
            }
        }
        "WEEKLY" => {
            while cursor_days * MS_PER_DAY <= horizon_end_ms {
                let ms = cursor_days * MS_PER_DAY + first_ms.rem_euclid(MS_PER_DAY);
                if !push(ms, &mut out) {
                    break;
                }
                cursor_days += 7 * interval;
            }
        }
        "MONTHLY" => {
            let (y0, m0, day_of_month) = civil_from_days(first_ms.div_euclid(MS_PER_DAY));
            let time_of_day = first_ms.rem_euclid(MS_PER_DAY);
            let mut index = 1_i64;
            loop {
                let total = (m0 as i64 - 1) + index * interval;
                let y = y0 + total.div_euclid(12);
                let m = (total.rem_euclid(12) + 1) as u32;
                let d = day_of_month.min(days_in_month(y, m));
                let ms = days_from_civil(y, m, d) * MS_PER_DAY + time_of_day;
                if ms > horizon_end_ms || index > 1200 {
                    break;
                }
                if !push(ms, &mut out) {
                    break;
                }
                index += 1;
            }
        }
        // YEARLY / SECONDLY / MINUTELY and unknown rules stay unexpanded —
        // the first occurrence still imports (documented limitation).
        _ => {}
    }
    out
}

// ---------------------------------------------------------------------------
// VCALENDAR parsing
// ---------------------------------------------------------------------------

struct RawEvent {
    props: Vec<(String, String)>,
    alarms: Vec<Vec<(String, String)>>,
}

fn collect_events(lines: &[String]) -> Vec<RawEvent> {
    let mut events = Vec::new();
    let mut current: Option<RawEvent> = None;
    let mut in_alarm = false;
    let mut alarm: Vec<(String, String)> = Vec::new();
    for line in lines {
        match split_property(line) {
            Some((name, value)) => match (name.as_str(), current.is_some()) {
                ("BEGIN", _) if value.eq_ignore_ascii_case("VEVENT") => {
                    current = Some(RawEvent {
                        props: Vec::new(),
                        alarms: Vec::new(),
                    });
                }
                ("BEGIN", _) if value.eq_ignore_ascii_case("VALARM") && current.is_some() => {
                    in_alarm = true;
                    alarm.clear();
                }
                ("END", _) if value.eq_ignore_ascii_case("VALARM") => {
                    if in_alarm {
                        if let Some(event) = current.as_mut() {
                            event.alarms.push(alarm.clone());
                        }
                    }
                    in_alarm = false;
                }
                ("END", _) if value.eq_ignore_ascii_case("VEVENT") => {
                    if let Some(event) = current.take() {
                        events.push(event);
                    }
                }
                (_, true) => {
                    let target: &mut Vec<(String, String)> = if in_alarm {
                        &mut alarm
                    } else {
                        match current.as_mut() {
                            Some(event) => &mut event.props,
                            None => continue,
                        }
                    };
                    target.push((name, unescape_text(&value)));
                }
                _ => {}
            },
            None => {}
        }
    }
    events
}

fn alarm_remind_before(alarms: &[Vec<(String, String)>]) -> Option<i64> {
    alarms
        .iter()
        .filter_map(|alarm| {
            alarm
                .iter()
                .find(|(name, _)| name == "TRIGGER")
                .map(|(_, value)| value.clone())
        })
        .filter_map(|trigger| parse_duration(&trigger))
        .filter(|ms| *ms < 0)
        .map(|ms| (-ms).min(crate::request::MAX_REMIND_BEFORE_MS))
        .min()
}

/// Parse an ICS payload into [`ParsedEvent`]s. Events without a usable
/// DTSTART are skipped (counted by the caller through the returned vec).
pub fn parse_ics(input: &str) -> Result<Vec<ParsedEvent>, String> {
    let lines = unfold_lines(input);
    let has_calendar = lines
        .iter()
        .any(|l| matches!(split_property(l), Some((name, _)) if name == "BEGIN"));
    if !has_calendar {
        return Err("no VCALENDAR/VEVENT components found".to_string());
    }
    let mut out = Vec::new();
    for raw in collect_events(&lines) {
        let get = |prop: &str| -> Option<String> {
            raw.props
                .iter()
                .find(|(name, _)| name == prop)
                .map(|(_, v)| v.clone())
        };
        let Some(start_value) = get("DTSTART") else {
            continue;
        };
        let (start_ms, all_day) = match parse_dt(&start_value) {
            Ok(parsed) => parsed,
            Err(error) => {
                eprintln!("[calendar] skipping event with bad DTSTART {start_value:?}: {error}");
                continue;
            }
        };
        let end_ms = match get("DTEND") {
            Some(value) => match parse_dt(&value) {
                Ok((ms, _)) => (ms >= start_ms).then_some(ms),
                Err(_) => None,
            },
            None => None,
        };
        let title = get("SUMMARY").unwrap_or_else(|| "(untitled)".to_string());
        let mut description = get("DESCRIPTION").unwrap_or_default();
        if let Some(location) = get("LOCATION") {
            if !location.is_empty() {
                description = format!("Location: {location}\n{description}");
            }
        }
        out.push(ParsedEvent {
            title,
            description,
            start_ms,
            end_ms,
            all_day,
            remind_before_ms: alarm_remind_before(&raw.alarms),
            uid: get("UID").unwrap_or_default(),
            rrule: get("RRULE"),
        });
    }
    Ok(out)
}

/// All occurrence start times of `event` within `[window_start_ms,
/// window_end_ms]`, including the first occurrence itself when inside.
pub fn occurrences(event: &ParsedEvent, window_start_ms: i64, window_end_ms: i64) -> Vec<i64> {
    if event.start_ms > window_end_ms {
        return Vec::new();
    }
    if event.rrule.is_none() {
        return (event.start_ms >= window_start_ms)
            .then_some(event.start_ms)
            .into_iter()
            .collect();
    }
    let rrule = event.rrule.as_deref().unwrap_or_default();
    let count_total = rrule.split(';').find_map(|part| {
        part.split_once('=')
            .filter(|(k, _)| k.eq_ignore_ascii_case("COUNT"))
            .and_then(|(_, v)| v.parse::<i64>().ok())
    });
    // COUNT includes the first occurrence; expansion generates the tail.
    let count_remaining = count_total.map(|c| (c - 1).max(0));
    let mut starts = Vec::new();
    if event.start_ms >= window_start_ms && event.start_ms <= window_end_ms {
        starts.push(event.start_ms);
    }
    starts.extend(expand_rrule(
        event.start_ms,
        rrule,
        window_end_ms,
        count_remaining,
        MAX_IMPORT_DOCS,
    ));
    starts.retain(|ms| *ms >= window_start_ms);
    starts
}

// ---------------------------------------------------------------------------
// VCALENDAR generation
// ---------------------------------------------------------------------------

const FOLD_AT: usize = 72;

/// Fold to ≤75 octets (RFC 5545 §3.1). Continuation segments must never
/// start with a space or tab — unfolding consumes exactly one WSP after the
/// CRLF as the marker, so a segment-initial content space would be eaten.
fn fold_line(line: &str) -> String {
    if line.len() <= FOLD_AT {
        return format!("{line}\r\n");
    }
    let bytes = line.as_bytes();
    let mut cut = |mut pos: usize, max: usize| -> usize {
        while pos > 1 && pos < max && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
            pos -= 1;
        }
        pos
    };
    let mut out = String::with_capacity(line.len() + line.len() / FOLD_AT * 3 + 2);
    let mut rest_start = cut(FOLD_AT.min(bytes.len() - 1), bytes.len());
    out.push_str(&line[..rest_start]);
    out.push_str("\r\n");
    while bytes.len() - rest_start > FOLD_AT - 1 {
        let seg_end = cut(rest_start + FOLD_AT - 1, bytes.len());
        out.push(' ');
        out.push_str(&line[rest_start..seg_end]);
        out.push_str("\r\n");
        rest_start = seg_end;
    }
    out.push(' ');
    out.push_str(&line[rest_start..]);
    out.push_str("\r\n");
    out
}

fn prop(name: &str, value: String) -> String {
    fold_line(&format!("{name}:{value}"))
}

/// Generate a VCALENDAR string for the given documents (already sorted by
/// the caller). All-day events emit `VALUE=DATE`; timed events emit UTC.
pub fn generate_ics(docs: &[crate::store::EventDoc]) -> String {
    let mut out = String::from(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//vynkor//calendar//EN\r\nCALSCALE:GREGORIAN\r\n",
    );
    for doc in docs {
        out.push_str("BEGIN:VEVENT\r\n");
        out.push_str(&prop(
            "UID",
            doc.ics_uid
                .clone()
                .unwrap_or_else(|| format!("cal-{}@vynkor.calendar", doc.id)),
        ));
        out.push_str(&prop("SUMMARY", escape_text(&doc.title)));
        if !doc.description.is_empty() {
            out.push_str(&prop("DESCRIPTION", escape_text(&doc.description)));
        }
        if doc.all_day {
            out.push_str(&format!(
                "DTSTART;VALUE=DATE:{}\r\n",
                fmt_dt(doc.start_ms, true)
            ));
            if let Some(end) = doc.end_ms {
                out.push_str(&format!("DTEND;VALUE=DATE:{}\r\n", fmt_dt(end, true)));
            }
        } else {
            out.push_str(&prop("DTSTART", fmt_dt(doc.start_ms, false)));
            if let Some(end) = doc.end_ms {
                out.push_str(&prop("DTEND", fmt_dt(end, false)));
            }
        }
        if let Some(remind) = doc.remind_before_ms {
            out.push_str("BEGIN:VALARM\r\nACTION:DISPLAY\r\n");
            out.push_str(&prop("TRIGGER", fmt_duration_before(remind)));
            out.push_str("END:VALARM\r\n");
        }
        out.push_str(&prop("DTSTAMP", fmt_dt(doc.updated_at_ms.max(0), false)));
        out.push_str("END:VEVENT\r\n");
    }
    out.push_str("END:VCALENDAR\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for &(y, m, d) in &[(1970, 1, 1), (2000, 2, 29), (2026, 8, 26), (1969, 12, 31)] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d), "{y}-{m}-{d}");
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(weekday_mon0(0), 3, "1970-01-01 was a Thursday");
        assert_eq!(weekday_mon0(days_from_civil(2026, 8, 26)), 2, "Wednesday");
    }

    #[test]
    fn parse_and_format_timestamps() {
        assert_eq!(parse_dt("19700101T000000Z").unwrap(), (0, false));
        assert_eq!(fmt_dt(0, false), "19700101T000000Z");
        let (ms, all_day) = parse_dt("20260826").unwrap();
        assert!(all_day);
        assert_eq!(fmt_dt(ms, true), "20260826");
        assert_eq!(
            parse_dt("20260826T103000").unwrap(),
            parse_dt("20260826T103000Z").unwrap(),
            "floating time reads as UTC"
        );
        assert!(parse_dt("20261340").is_err(), "month 13 rejected");
        assert!(parse_dt("20260230").is_err(), "Feb 30 rejected");
        assert!(parse_dt("xx").is_err());
    }

    #[test]
    fn text_escape_round_trip() {
        let raw = "back\\slash;semi,comma\nnewline";
        assert_eq!(unescape_text(&escape_text(raw)), raw);
    }

    #[test]
    fn unfolds_folded_lines() {
        let folded = "SUMMARY:a very long title that\r\n continues here\r\nDESCRIPTION:x";
        let lines = unfold_lines(folded);
        // RFC 5545 unfolding consumes CRLF + exactly one WSP marker, so the
        // content space is NOT restored — generators must not fold in front
        // of a content space (fold_line below guarantees that).
        assert_eq!(lines[0], "SUMMARY:a very long title thatcontinues here");
        assert_eq!(lines[1], "DESCRIPTION:x");
    }

    #[test]
    fn splits_property_with_quoted_param_colon() {
        let (name, value) =
            split_property("DTSTART;TZID=\"Europe/Berlin\":20260826T100000").unwrap();
        assert_eq!(name, "DTSTART");
        assert_eq!(value, "20260826T100000");
    }

    #[test]
    fn parses_trigger_durations() {
        assert_eq!(parse_duration("-PT15M"), Some(-900_000));
        assert_eq!(parse_duration("-PT1H"), Some(-3_600_000));
        assert_eq!(parse_duration("-P1D"), Some(-MS_PER_DAY));
        assert_eq!(parse_duration("-P1DT30M"), Some(-(MS_PER_DAY + 1_800_000)));
        assert_eq!(parse_duration("-PT90S"), Some(-90_000));
        assert_eq!(parse_duration("PT15M"), Some(900_000));
        assert_eq!(
            parse_duration("20260826T100000Z"),
            None,
            "absolute trigger ignored"
        );
    }

    fn sample_event(extra: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n\
             BEGIN:VEVENT\r\nUID:evt-1@example.com\r\nDTSTAMP:20260801T000000Z\r\n\
             DTSTART:20260901T090000Z\r\nDTEND:20260901T100000Z\r\n\
             SUMMARY:Dentist\r\nLOCATION:Main St 1\r\nDESCRIPTION:checkup\\, bring card\r\n\
             {extra}\
             END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    #[test]
    fn parses_simple_event_with_alarm() {
        let ics =
            sample_event("BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\n");
        let events = parse_ics(&ics).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.title, "Dentist");
        assert_eq!(event.uid, "evt-1@example.com");
        assert!(
            event.description.starts_with("Location: Main St 1\n"),
            "{}",
            event.description
        );
        assert!(event.description.ends_with("checkup, bring card"));
        assert_eq!(event.remind_before_ms, Some(900_000));
        assert!(!event.all_day);
        let expected_start = parse_dt("20260901T090000Z").unwrap().0;
        let expected_end = parse_dt("20260901T100000Z").unwrap().0;
        assert_eq!(event.start_ms, expected_start);
        assert_eq!(event.end_ms, Some(expected_end));
    }

    #[test]
    fn skips_event_without_dtstart() {
        let ics =
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:no time\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(parse_ics(ics).unwrap().is_empty());
    }

    #[test]
    fn rejects_payload_without_components() {
        assert!(parse_ics("just some text").is_err());
    }

    #[test]
    fn non_recurring_single_occurrence_in_window() {
        let events = parse_ics(&sample_event("")).unwrap();
        let window_start = parse_dt("20260826T000000Z").unwrap().0;
        let window_end = parse_dt("20261124T000000Z").unwrap().0;
        let starts = occurrences(&events[0], window_start, window_end);
        assert_eq!(starts, vec![events[0].start_ms]);
    }

    #[test]
    fn weekly_byday_expands_within_horizon() {
        let extra = "RRULE:FREQ=WEEKLY;BYDAY=TU,TH\r\n";
        let events = parse_ics(&sample_event(extra)).unwrap();
        let event = &events[0];
        // 2026-09-01 is a Tuesday.
        let window_start = parse_dt("20260901T000000Z").unwrap().0;
        let window_end = parse_dt("20261001T000000Z").unwrap().0;
        let starts = occurrences(event, window_start, window_end);
        assert_eq!(
            starts.len(),
            9,
            "Sep has Tue/Thu on 1,3,8,10,15,17,22,24,29: {starts:?}"
        );
        for pair in starts.windows(2) {
            let gap_days = (pair[1] - pair[0]) / MS_PER_DAY;
            assert!(
                gap_days == 2 || gap_days == 5,
                "gap {gap_days} between {pair:?}"
            );
        }
    }

    #[test]
    fn daily_interval_respects_count() {
        let extra = "RRULE:FREQ=DAILY;COUNT=4\r\n";
        let events = parse_ics(&sample_event(extra)).unwrap();
        let window_start = parse_dt("20260901T000000Z").unwrap().0;
        let window_end = parse_dt("20270101T000000Z").unwrap().0;
        let starts = occurrences(&events[0], window_start, window_end);
        assert_eq!(
            starts.len(),
            4,
            "COUNT=4 caps expansion including the first: {starts:?}"
        );
    }

    #[test]
    fn monthly_clamps_short_months() {
        let ics = sample_event("RRULE:FREQ=MONTHLY\r\n")
            .replace("DTSTART:20260901T090000Z", "DTSTART:20260131T090000Z")
            .replace("DTEND:20260901T100000Z", "DTEND:20260131T100000Z");
        let events = parse_ics(&ics).unwrap();
        let window_start = parse_dt("20260101T000000Z").unwrap().0;
        let window_end = parse_dt("20260701T000000Z").unwrap().0;
        let starts = occurrences(&events[0], window_start, window_end);
        let months: Vec<(i64, u32)> = starts
            .iter()
            .map(|ms| civil_from_days(ms.div_euclid(MS_PER_DAY)))
            .map(|(y, m, _)| (y, m))
            .collect();
        assert_eq!(
            months,
            vec![
                (2026, 1),
                (2026, 2),
                (2026, 3),
                (2026, 4),
                (2026, 5),
                (2026, 6)
            ]
        );
    }

    #[test]
    fn yearly_rule_stays_unexpanded() {
        let extra = "RRULE:FREQ=YEARLY\r\n";
        let events = parse_ics(&sample_event(extra)).unwrap();
        let window_start = parse_dt("20260826T000000Z").unwrap().0;
        let window_end = parse_dt("20300101T000000Z").unwrap().0;
        assert_eq!(occurrences(&events[0], window_start, window_end).len(), 1);
    }

    #[test]
    fn generation_is_parseable_and_folds_long_lines() {
        let doc = crate::store::EventDoc {
            id: "7".to_string(),
            title: "long ".repeat(40),
            description: "line1\nline2; with, specials \\".to_string(),
            start_ms: 1_787_800_000_000,
            end_ms: Some(1_787_803_600_000),
            all_day: false,
            remind_before_ms: Some(900_000),
            reminder_fired: false,
            tags: vec![],
            created_at_ms: 0,
            updated_at_ms: 1_787_700_000_000,
            ics_uid: Some("round-trip@test".to_string()),
            rrule: None,
        };
        let ics = generate_ics(&[doc.clone()]);
        assert!(ics.contains("UID:round-trip@test"));
        assert!(ics.lines().all(|l| l.len() <= 75), "folded to 75 octets");
        assert!(ics.contains("\\nline2\\; with\\, specials \\\\"));
        let parsed = parse_ics(&ics).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, doc.title);
        assert_eq!(parsed[0].description, doc.description);
        assert_eq!(parsed[0].start_ms, doc.start_ms);
        assert_eq!(parsed[0].end_ms, doc.end_ms);
        assert_eq!(parsed[0].remind_before_ms, Some(900_000));
        assert_eq!(parsed[0].uid, "round-trip@test");
    }

    #[test]
    fn all_day_generation_uses_date_form() {
        let doc = crate::store::EventDoc {
            id: "1".to_string(),
            title: "Offsite".to_string(),
            description: String::new(),
            start_ms: days_from_civil(2026, 9, 10) * MS_PER_DAY,
            end_ms: Some(days_from_civil(2026, 9, 12) * MS_PER_DAY),
            all_day: true,
            remind_before_ms: None,
            reminder_fired: false,
            tags: vec![],
            created_at_ms: 0,
            updated_at_ms: 0,
            ics_uid: None,
            rrule: None,
        };
        let ics = generate_ics(&[doc]);
        assert!(ics.contains("DTSTART;VALUE=DATE:20260910"));
        assert!(ics.contains("DTEND;VALUE=DATE:20260912"));
        assert!(
            ics.contains("UID:cal-1@vynkor.calendar"),
            "fallback uid from id"
        );
        assert!(!ics.contains("VALARM"));
    }
}
