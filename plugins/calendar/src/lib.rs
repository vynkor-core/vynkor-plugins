//! `calendar` plugin library crate: event CRUD over the `database` plugin
//! plus pure reminder-selection logic.
//!
//! Thin schema layer (root `ROADMAP.md`): every event is one JSON document
//! stored under `event:<id>` in this plugin's own `database` namespace, ids
//! come from an atomic `db_incr` counter (`meta:next_id`). No local state —
//! restart-safe by construction; reminders that came due while the plugin
//! was down fire once on the next scan with `late: true`.
//!
//! Outbound calls go through [`Rpc`], a channel-fronted proxy: handler and
//! scan tasks never touch the `VynkorClient` directly, because `send_action`
//! discards every non-matching inbound frame while it waits — a scan started
//! by the timer would silently eat user requests arriving mid-scan. With the
//! proxy the serve loop stays the single reader: user requests, pings and
//! publish-acks are always matched and routed, nothing is dropped (same
//! rationale as sync-client's custom loop).

pub mod ics;
pub mod reminders;
pub mod request;
pub mod store;

use std::collections::HashMap;

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{envelope, Envelope, EventPublish};

use reminders::DueFire;
use rrule::DateFilter;
use std::str::FromStr;
use store::EventDoc;

/// Runtime configuration (environment-driven; see `config.example.yaml`).
#[derive(Debug, Clone)]
pub struct Config {
    /// Per-call timeout for `database` IPC round-trips.
    pub db_timeout_ms: u32,
    /// Reminder scan interval in seconds; `0` disables scanning entirely.
    pub scan_secs: u64,
    /// Deliver fired reminders through the `notify` plugin as well
    /// (best-effort — a notify failure never fails the scan).
    pub notify_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_timeout_ms: 5000,
            scan_secs: 30,
            notify_enabled: true,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let db_timeout_ms = std::env::var("CALENDAR_PLUGIN_DB_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5000);
        let scan_secs = std::env::var("CALENDAR_PLUGIN_SCAN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let notify_enabled = std::env::var("CALENDAR_PLUGIN_NOTIFY")
            .ok()
            .map(|s| {
                let s = s.trim().to_ascii_lowercase();
                !s.is_empty() && s != "false" && s != "0"
            })
            .unwrap_or(true);
        Self {
            db_timeout_ms,
            scan_secs,
            notify_enabled,
        }
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
/// (`database`, `notify`). Every [`Rpc::call`] round-trips through the serve
/// loop's single `recv()` point.
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
            .send(RpcCall {
                action: action.to_string(),
                params_json,
                timeout_ms,
                reply,
            })
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

/// Best-effort change event to publish AFTER the action response is sent
/// (the kernel namespaces the type to `plugin.calendar.changed` / `.due`).
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

/// Handle one kernel-routed action. Storage failures (including "not found"
/// on update) surface as `Err` → `ACTION_ERROR`; reading a missing event is
/// a `{"found": false}` result, deleting a missing one a `{"deleted": false}`
/// result — neither is an error.
pub async fn handle_action(
    rpc: Rpc,
    config: &Config,
    action: &str,
    params_json: &[u8],
) -> Result<ActionResult, String> {
    let req = request::parse_request(action, params_json)?;
    let db = store::Db::new(rpc, config.db_timeout_ms);
    match req {
        request::CalendarRequest::Create(e) => {
            let id = db.next_id().await?.to_string();
            let now = store::now_ms();
            let doc = EventDoc {
                id: id.clone(),
                title: e.title,
                description: e.description,
                start_ms: e.start_ms,
                end_ms: e.end_ms,
                all_day: e.all_day,
                remind_before_ms: e.remind_before_ms,
                reminder_fired: false,
                tags: e.tags,
                ics_uid: None,
                rrule: e.rrule,
                created_at_ms: now,
                updated_at_ms: now,
            };
            db.put(&doc).await?;
            ok(
                json!({"id": id, "event": doc}),
                Some(changed("created", &doc.id)),
            )
        }
        request::CalendarRequest::Get { id } => match db.get(&id).await? {
            Some(doc) => ok(json!({"found": true, "event": doc}), None),
            None => ok(json!({"found": false, "event": null}), None),
        },
        request::CalendarRequest::List {
            from_ms,
            to_ms,
            tag,
            limit,
            offset,
        } => {
            let events = db.list().await?;
            let mut expanded = Vec::new();
            for e in events {
                if let Some(rrule) = &e.rrule {
                    let window_from = from_ms.unwrap_or_else(|| store::now_ms() - 24 * 3600 * 1000);
                    let window_to =
                        to_ms.unwrap_or_else(|| store::now_ms() + 90 * 24 * 3600 * 1000);
                    let occs = expand_rrule(&e, rrule, window_from, window_to);
                    for occ in occs {
                        if let Some(tag) = &tag {
                            if !occ.tags.iter().any(|t| t == tag) {
                                continue;
                            }
                        }
                        expanded.push(occ);
                    }
                } else {
                    if let Some(from) = from_ms {
                        if e.start_ms < from {
                            continue;
                        }
                    }
                    if let Some(to) = to_ms {
                        if e.start_ms > to {
                            continue;
                        }
                    }
                    if let Some(tag) = &tag {
                        if !e.tags.iter().any(|t| t == tag) {
                            continue;
                        }
                    }
                    expanded.push(e);
                }
            }
            expanded.sort_by(|a, b| {
                a.start_ms
                    .cmp(&b.start_ms)
                    .then_with(|| id_num(a).cmp(&id_num(b)))
            });
            let total = expanded.len();
            let page: Vec<&EventDoc> = expanded.iter().skip(offset).take(limit).collect();
            ok(json!({"events": page, "total": total}), None)
        }
        request::CalendarRequest::Update { id, patch } => {
            let mut doc = db
                .get(&id)
                .await?
                .ok_or_else(|| format!("event not found: {id}"))?;
            if let Some(t) = patch.title {
                doc.title = t;
            }
            if let Some(d) = patch.description {
                doc.description = d;
            }
            // Any change to the time shape invalidates a previously fired
            // reminder — the new schedule deserves a new notification.
            let mut times_changed = false;
            if let Some(s) = patch.start_ms {
                if doc.start_ms != s {
                    times_changed = true;
                }
                doc.start_ms = s;
            }
            if let Some(e) = patch.end_ms {
                if doc.end_ms != Some(e) {
                    times_changed = true;
                }
                doc.end_ms = Some(e);
            }
            if let Some(a) = patch.all_day {
                doc.all_day = a;
            }
            if let Some(rb) = patch.remind_before_ms {
                if doc.remind_before_ms != Some(rb) {
                    times_changed = true;
                }
                doc.remind_before_ms = Some(rb);
            }
            if let Some(t) = patch.tags {
                doc.tags = t;
            }
            if let Some(r) = patch.rrule {
                if doc.rrule != r {
                    times_changed = true;
                }
                doc.rrule = r;
            }
            if let Some(end) = doc.end_ms {
                if end < doc.start_ms {
                    return Err(format!(
                        "resulting end_ms must be >= start_ms ({} < {})",
                        end, doc.start_ms
                    ));
                }
            }
            if times_changed {
                doc.reminder_fired = false;
            }
            doc.updated_at_ms = store::now_ms();
            db.put(&doc).await?;
            ok(
                json!({"updated": true, "event": doc}),
                Some(changed("updated", &doc.id)),
            )
        }
        request::CalendarRequest::Delete { id } => {
            let deleted = db.delete(&id).await?;
            let event = deleted.then(|| changed("deleted", &id));
            ok(json!({"deleted": deleted}), event)
        }
        request::CalendarRequest::Import { ics_base64 } => import_ics(&db, &ics_base64).await,
        request::CalendarRequest::Export => export_ics(&db).await,
    }
}

/// Decode, parse and upsert one ICS payload keyed by `UID`. Events inside the
/// import window (`[now-1d, now+horizon]`) are created or updated in place —
/// a re-import of the same calendar never duplicates events. RRULEs expand to
/// concrete occurrences (first-stage semantics from PLANS.md EXI-01).
async fn import_ics(db: &store::Db, ics_base64: &str) -> Result<ActionResult, String> {
    let compact: String = ics_base64
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .map_err(|e| format!("params.ics_base64 is not valid base64: {e}"))?;
    let text = String::from_utf8(decoded)
        .map_err(|_| "params.ics_base64 does not decode to UTF-8 text".to_string())?;
    let parsed = ics::parse_ics(&text)?;
    if parsed.is_empty() {
        return Err("no VEVENT entries found in the calendar".to_string());
    }

    let now = store::now_ms();
    let window_start = now - ics::MS_PER_DAY;
    let window_end = now.saturating_add(ics::IMPORT_HORIZON_DAYS * ics::MS_PER_DAY);

    let existing = db.list().await?;
    let mut by_uid: HashMap<String, EventDoc> = existing
        .into_iter()
        .filter_map(|doc| doc.ics_uid.clone().map(|uid| (uid, doc)))
        .collect();

    let mut imported: u64 = 0;
    let mut updated: u64 = 0;
    let mut touched_ids: Vec<String> = Vec::new();
    let mut docs_written: usize = 0;

    for event in &parsed {
        for start in ics::occurrences(event, window_start, window_end) {
            if docs_written >= ics::MAX_IMPORT_DOCS {
                return Err(format!(
                    "import exceeds {} documents — narrow the calendar or the window",
                    ics::MAX_IMPORT_DOCS
                ));
            }
            let duration = event.end_ms.map(|end| end - event.start_ms);
            match by_uid.get_mut(&event.uid) {
                Some(doc) if !event.uid.is_empty() => {
                    if doc.start_ms != start || doc.title != event.title {
                        doc.reminder_fired = false;
                    }
                    doc.title = event.title.clone();
                    doc.description = event.description.clone();
                    doc.start_ms = start;
                    doc.end_ms = duration.map(|d| start + d);
                    doc.all_day = event.all_day;
                    doc.remind_before_ms = event.remind_before_ms;
                    doc.updated_at_ms = store::now_ms();
                    db.put(doc).await?;
                    touched_ids.push(doc.id.clone());
                    updated += 1;
                }
                _ => {
                    let id = db.next_id().await?.to_string();
                    let doc = EventDoc {
                        id: id.clone(),
                        title: event.title.clone(),
                        description: event.description.clone(),
                        start_ms: start,
                        end_ms: duration.map(|d| start + d),
                        all_day: event.all_day,
                        remind_before_ms: event.remind_before_ms,
                        reminder_fired: false,
                        tags: Vec::new(),
                        ics_uid: (!event.uid.is_empty()).then(|| event.uid.clone()),
                        rrule: None,
                        created_at_ms: now,
                        updated_at_ms: now,
                    };
                    db.put(&doc).await?;
                    if !event.uid.is_empty() {
                        by_uid.insert(event.uid.clone(), doc);
                    }
                    touched_ids.push(id);
                    imported += 1;
                }
            }
            docs_written += 1;
        }
    }
    ok(
        json!({
            "imported": imported,
            "updated": updated,
            "parsed": parsed.len(),
            "window_days": ics::IMPORT_HORIZON_DAYS,
            "event_ids": touched_ids,
        }),
        Some(ChangeEvent {
            event_type: "changed",
            payload: json!({"op": "imported", "imported": imported, "updated": updated}),
        }),
    )
}

async fn export_ics(db: &store::Db) -> Result<ActionResult, String> {
    let mut events = db.list().await?;
    events.sort_by(|a, b| a.start_ms.cmp(&b.start_ms));
    let text = ics::generate_ics(&events);
    ok(
        json!({"ics_base64": base64::engine::general_purpose::STANDARD.encode(text)}),
        None,
    )
}

/// Scan all events once and fire every due reminder. Per reminder, in order:
/// mark fired FIRST (at-most-once — a crash between mark and publish loses
/// one notification, while the reverse order would re-fire forever), queue
/// the `due` event fire-and-forget, then best-effort `notify_send`. A failure
/// in publish/notify never aborts the remaining reminders.
pub async fn scan_due(
    rpc: Rpc,
    outbound: mpsc::Sender<Envelope>,
    config: &Config,
    now_ms: i64,
) -> Result<usize, String> {
    let fires = {
        let db = store::Db::new(rpc.clone(), config.db_timeout_ms);
        let events = db.list().await?;
        reminders::due_events(&events, now_ms)
    };

    for fire in &fires {
        {
            let db = store::Db::new(rpc.clone(), config.db_timeout_ms);
            let mut doc = fire.event.clone();
            doc.reminder_fired = true;
            db.put(&doc).await?;
        }

        let payload = json!({
            "event_id": fire.event.id,
            "title": fire.event.title,
            "start_ms": fire.event.start_ms,
            "remind_at_ms": fire.remind_at_ms,
            "late": fire.late,
        });
        let publish = Envelope {
            payload: Some(envelope::Payload::EventPublish(EventPublish {
                event_type: "due".into(),
                payload_json: payload.to_string().into_bytes(),
            })),
            ..Default::default()
        };
        if let Err(e) = outbound.send(publish).await {
            eprintln!("[calendar] due publish failed: {e}");
        }

        if config.notify_enabled {
            let params = json!({
                "title": "Calendar",
                "message": reminder_message(fire, now_ms),
            });
            if let Err(e) = send_notify(&rpc, config.db_timeout_ms, &params).await {
                eprintln!("[calendar] notify_send failed: {e}");
            }
        }
    }
    Ok(fires.len())
}

async fn send_notify(rpc: &Rpc, timeout_ms: u32, params: &Value) -> Result<(), String> {
    rpc.call("notify_send", params.clone(), timeout_ms)
        .await
        .map(|_| ())
}

fn reminder_message(fire: &DueFire, now_ms: i64) -> String {
    if fire.late {
        format!("{} — missed reminder (started earlier)", fire.event.title)
    } else if fire.event.remind_before_ms == Some(0) {
        format!("{} starts now", fire.event.title)
    } else {
        let minutes = (fire.event.start_ms - now_ms) / 60_000;
        if minutes >= 1 {
            format!("{} starts in ~{minutes} min", fire.event.title)
        } else {
            format!("{} starts now", fire.event.title)
        }
    }
}

fn id_num(e: &EventDoc) -> u64 {
    e.id.parse::<u64>().unwrap_or(0)
}

fn expand_rrule(master: &EventDoc, rrule: &str, window_from: i64, window_to: i64) -> Vec<EventDoc> {
    use chrono::TimeZone;
    use std::str::FromStr;
    let dtstart = chrono::Utc
        .timestamp_millis_opt(master.start_ms)
        .single()
        .unwrap_or_else(|| chrono::Utc.timestamp_millis_opt(0).single().unwrap());
    let rrule_str = format!(
        "DTSTART:{}\nRRULE:{}",
        dtstart.format("%Y%m%dT%H%M%SZ"),
        rrule
    );
    let set = match rrule::RRuleSet::from_str(&rrule_str) {
        Ok(s) => s,
        Err(_) => return vec![master.clone()],
    };
    let mut out = Vec::new();
    let duration = master.end_ms.map(|end| end - master.start_ms);
    let dates = match set.all(50) {
        Ok(v) => v,
        Err(_) => return vec![master.clone()],
    };
    for dt in dates {
        let ms = dt.timestamp_millis();
        if ms < window_from || ms > window_to {
            continue;
        }
        let mut occ = master.clone();
        occ.id = format!("{}#{}", master.id, ms);
        occ.start_ms = ms;
        occ.end_ms = duration.map(|d| ms + d);
        out.push(occ);
        if out.len() >= 50 {
            break;
        }
    }
    if out.is_empty() {
        vec![master.clone()]
    } else {
        out
    }
}

fn ok(data: Value, event: Option<ChangeEvent>) -> Result<ActionResult, String> {
    let data = serde_json::to_vec(&data).map_err(|e| format!("failed to encode response: {e}"))?;
    Ok(ActionResult { data, event })
}

fn changed(op: &'static str, id: &str) -> ChangeEvent {
    ChangeEvent {
        event_type: "changed",
        payload: json!({"op": op, "id": id}),
    }
}
