//! Nightly mining: find goals that were solved by one eligible tool call,
//! group the repeats, and let a local model judge each group and write its
//! paraphrases and replies.
//!
//! Candidate extraction is pure ([`candidates`]); the model only enriches
//! (see [`parse_enrichment`]) — it never decides which tool call to make,
//! and [`guard`] filters what it writes before [`merge`] stores any of it.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::guard::{self, Polarity};
use super::{
    allowed_tools, eligible, mining_agent_id, spec_hash, store, Book, Routine, STATUS_ACTIVE,
};
use crate::llm::{self, CallTag};
use crate::smalltalk::normalize;
use crate::store::{now_ms, Db, LlmPlan, MiningRow, Turn, STATUS_COMPLETED};
use crate::tools::Catalog;
use crate::Rpc;

/// Successes of one identical tool call needed before it is learned.
pub const MIN_SUCCESSES: u32 = 2;
pub const MAX_PARAPHRASES: usize = 8;
pub const MAX_REPLIES: usize = 6;
/// Fewer surviving model replies than this are topped up from
/// [`guard::stock_replies`].
const MIN_REPLIES: usize = 3;
/// Replies taken from the model before [`guard::reply_ok`] thins them out.
const MAX_REPLY_CANDIDATES: usize = 12;
pub const MAX_REPLY_CHARS: usize = 160;

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub tool: String,
    pub params: Value,
    /// Normalized, unique, sorted.
    pub texts: Vec<String>,
    pub successes: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Enrichment {
    /// The model's verdict: the phrasings stand alone and mean exactly this
    /// call. Missing reads as `false`.
    pub learn: bool,
    pub reason: String,
    pub paraphrases: Vec<String>,
    pub replies: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    Created,
    Updated,
    Dropped,
}

/// Key-sorted JSON text, so `{"a":1,"b":2}` and `{"b":2,"a":1}` group
/// together whatever map ordering serde_json was built with.
fn canonical(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let s: BTreeMap<&String, Value> = m.iter().map(|(k, v)| (k, sorted(v))).collect();
                json!(s)
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(v).to_string()
}

fn same_call(r: &Routine, tool: &str, params: &Value) -> bool {
    r.tool == tool && canonical(&r.params) == canonical(params)
}

/// The single successful tool call of a goal that qualifies for learning.
fn single_call(goal: &MiningRow) -> Option<(&str, &Value)> {
    if goal.status != STATUS_COMPLETED || goal.steps.iter().any(|s| s.kind == "tool_error") {
        return None;
    }
    let mut ok = goal.steps.iter().filter(|s| s.kind == "tool_ok");
    let step = ok.next()?;
    if ok.next().is_some() {
        return None;
    }
    Some((
        step.detail.get("tool")?.as_str()?,
        step.detail.get("params")?,
    ))
}

pub fn candidates(
    goals: &[MiningRow],
    catalog: &Catalog,
    allow: &[String],
    book: &Book,
) -> Vec<Candidate> {
    let mut groups: BTreeMap<(String, String), Candidate> = BTreeMap::new();
    for goal in goals {
        let Some((tool, params)) = single_call(goal) else {
            continue;
        };
        if !catalog.get(tool).is_some_and(|s| eligible(s, allow))
            || guard::is_read_call(tool, params)
        {
            continue;
        }
        let text = normalize(&goal.goal);
        if text.is_empty() {
            continue;
        }
        // a disabled routine is only re-learned from goals after it was disabled
        let stale = book.routines.iter().any(|r| {
            !r.is_active() && same_call(r, tool, params) && goal.updated_at_ms <= r.disabled_at_ms
        });
        if stale {
            continue;
        }
        let c = groups
            .entry((tool.to_string(), canonical(params)))
            .or_insert_with(|| Candidate {
                tool: tool.to_string(),
                params: params.clone(),
                texts: Vec::new(),
                successes: 0,
            });
        c.successes += 1;
        if !c.texts.contains(&text) {
            c.texts.push(text);
        }
    }

    // the same words that led to two different calls are ambiguous everywhere
    let mut owners: BTreeMap<String, usize> = BTreeMap::new();
    for c in groups.values() {
        for t in &c.texts {
            *owners.entry(t.clone()).or_default() += 1;
        }
    }
    groups
        .into_values()
        .filter_map(|mut c| {
            let polarity = guard::call_polarity(&c.params);
            c.texts
                .retain(|t| owners[t] == 1 && guard::phrase_ok(t, polarity));
            c.texts.sort();
            (c.successes >= MIN_SUCCESSES && !c.texts.is_empty()).then_some(c)
        })
        .collect()
}

pub fn parse_enrichment(content: &str) -> Option<Enrichment> {
    let text = llm::strip_tool_markup(content);
    let v: Value = serde_json::from_str(llm::first_balanced_object(&text)?).ok()?;
    let strings = |k: &str| -> Vec<String> {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut replies: Vec<String> = strings("replies")
        .into_iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty() && r.chars().count() <= MAX_REPLY_CHARS)
        .collect();
    replies.dedup();
    replies.truncate(MAX_REPLY_CANDIDATES);
    if replies.is_empty() {
        return None;
    }
    let mut paraphrases: Vec<String> = strings("paraphrases")
        .iter()
        .map(|p| normalize(p))
        .filter(|p| !p.is_empty())
        .collect();
    paraphrases.dedup();
    paraphrases.truncate(MAX_PARAPHRASES);
    Some(Enrichment {
        learn: v.get("learn").and_then(Value::as_bool).unwrap_or(false),
        reason: v
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect(),
        paraphrases,
        replies,
    })
}

/// The model decides whether to learn; [`guard`] only vetoes what it
/// writes that would act or answer wrongly.
pub fn merge(
    book: &mut Book,
    c: &Candidate,
    e: &Enrichment,
    spec_hash: &str,
    now_ms: i64,
) -> MergeOutcome {
    if !e.learn {
        return MergeOutcome::Dropped;
    }
    let polarity = guard::call_polarity(&c.params);
    let mut replies: Vec<String> = e
        .replies
        .iter()
        .filter(|r| guard::reply_ok(r, polarity, &c.texts))
        .take(MAX_REPLIES)
        .cloned()
        .collect();
    for stock in guard::stock_replies(polarity) {
        if replies.len() >= MIN_REPLIES {
            break;
        }
        if !replies.iter().any(|r| r == stock) {
            replies.push(stock.to_string());
        }
    }
    let existing = book
        .routines
        .iter()
        .position(|r| same_call(r, &c.tool, &c.params));
    let owned_elsewhere = |p: &String| {
        book.routines
            .iter()
            .enumerate()
            .any(|(i, r)| Some(i) != existing && r.phrases.contains(p))
    };
    let mut phrases: Vec<String> = Vec::new();
    let paraphrases = e
        .paraphrases
        .iter()
        .filter(|p| guard::phrase_ok(p, polarity) && guard::anchored(p, &c.texts));
    for p in c.texts.iter().chain(paraphrases) {
        if !phrases.contains(p) && !owned_elsewhere(p) {
            phrases.push(p.clone());
        }
    }
    if phrases.is_empty() {
        return MergeOutcome::Dropped;
    }
    match existing {
        Some(i) => {
            let r = &mut book.routines[i];
            for p in phrases {
                if !r.phrases.contains(&p) {
                    r.phrases.push(p);
                }
            }
            r.replies = replies;
            r.last_reply_idx = 0;
            r.spec_hash = spec_hash.to_string();
            r.status = STATUS_ACTIVE.to_string();
            r.disabled_reason.clear();
            r.updated_at_ms = now_ms;
            MergeOutcome::Updated
        }
        None => {
            book.routines.push(Routine {
                id: format!("r{now_ms}-{}", book.routines.len()),
                tool: c.tool.clone(),
                params: c.params.clone(),
                phrases,
                replies,
                spec_hash: spec_hash.to_string(),
                status: STATUS_ACTIVE.to_string(),
                disabled_reason: String::new(),
                disabled_at_ms: 0,
                hits: 0,
                fails: 0,
                last_reply_idx: 0,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
            });
            MergeOutcome::Created
        }
    }
}

fn mining_prompt(c: &Candidate, spec: &crate::tools::ToolSpec) -> String {
    let seen: Vec<String> = c.texts.iter().map(|t| format!("- {t}")).collect();
    let effect = match guard::call_polarity(&c.params) {
        Some(Polarity::On) => "\nThis call switches the device ON.",
        Some(Polarity::Off) => "\nThis call switches the device OFF.",
        None => "",
    };
    format!(
        "{persona}\n\n\
         Task: prepare an instant shortcut for a home-device command. The user \
         gave this command {n} times, and each time exactly this tool call did it:\n\
         tool: {name}\ndescription: {desc}\nparams: {params}{effect}\n\
         User phrasings seen:\n{seen}\n\n\
         Return ONLY one JSON object:\n\
         {{\"learn\": true, \"reason\": \"...\", \"paraphrases\": [\"...\"], \"replies\": [\"...\"]}}\n\
         - learn: true only if every phrasing above, said with no earlier \
         conversation, means exactly this call: no pointing back (\"его\", \"тоже\", \
         \"ещё раз\"), no ambiguity about which device or what to do, and the \
         confirmation does not depend on the result (a question about state is \
         false). When in doubt, false.\n\
         - reason: one short sentence why.\n\
         - paraphrases: up to {max_p} other short ways the user would say exactly this \
         command, in the user's language. Never the opposite action, never another \
         device or value.\n\
         - replies: 8-10 different short confirmations that the action is DONE, each \
         naming the device with the user's own word for it and its new state, in \
         character and in the user's language, at most 120 characters, natural both as \
         text and spoken aloud, no emoji.",
        persona = llm::persona_block(),
        n = c.successes,
        name = spec.name,
        desc = spec.description,
        params = c.params,
        effect = effect,
        seen = seen.join("\n"),
        max_p = MAX_PARAPHRASES,
    )
}

/// One mining pass. Model calls happen first; the book is re-read right
/// before merging so hits recorded meanwhile by live goals are not lost.
pub async fn run(rpc: &Rpc, db: &Db, catalog: &Catalog) -> Result<Value, String> {
    let agent_id = mining_agent_id().ok_or_else(|| {
        "routine mining needs a local ai profile: set AGENT_PLUGIN_ROUTINES_AGENT_ID \
         (or AGENT_PLUGIN_MEMORY_AGENT_ID)"
            .to_string()
    })?;
    let plan = LlmPlan {
        provider: String::new(),
        base_url: String::new(),
        model: String::new(),
        api_key_env: String::new(),
        agent_id,
        max_tokens: 1024,
    };
    let since = now_ms() - super::lookback_days() * 86_400_000;
    let goals = db.mining_rows(since).await?;
    let allow = allowed_tools();
    let found = candidates(
        &goals,
        catalog,
        &allow,
        &store::load(rpc, db.timeout_ms()).await,
    );

    let mut enriched = Vec::new();
    let mut skipped = 0;
    for c in &found {
        let Some(spec) = catalog.get(&c.tool) else {
            continue;
        };
        let transcript = [Turn {
            role: "user".into(),
            content: mining_prompt(c, spec),
        }];
        let tag = CallTag {
            purpose: "routine_mine",
            goal_id: "",
        };
        // no fallback on purpose: the fallback profile may be a cloud one
        match llm::chat(rpc, &plan, &transcript, &[], &tag).await {
            Ok(o) => match parse_enrichment(&o.content) {
                Some(e) => {
                    eprintln!(
                        "[agent] routines: model on {} {:?}: learn={} ({}) paraphrases={:?} replies={:?}",
                        c.tool, c.texts, e.learn, e.reason, e.paraphrases, e.replies
                    );
                    enriched.push((c, e, spec_hash(spec)))
                }
                None => {
                    eprintln!(
                        "[agent] routines: unusable mining reply for {}: {}",
                        c.tool, o.content
                    );
                    skipped += 1;
                }
            },
            Err(e) => {
                eprintln!("[agent] routines: mining call failed for {}: {e}", c.tool);
                skipped += 1;
            }
        }
    }

    let now = now_ms();
    let (created, updated, dropped) = store::update(rpc, db.timeout_ms(), |book| {
        let (mut created, mut updated, mut dropped) = (0, 0, 0);
        for (c, e, hash) in &enriched {
            match merge(book, c, e, hash, now) {
                MergeOutcome::Created => created += 1,
                MergeOutcome::Updated => updated += 1,
                MergeOutcome::Dropped => {
                    eprintln!(
                        "[agent] routines: dropped {} {:?}: learn={}",
                        c.tool, c.texts, e.learn
                    );
                    dropped += 1
                }
            }
        }
        (created + updated > 0, (created, updated, dropped))
    })
    .await?;
    let summary = json!({
        "candidates": found.len(),
        "created": created,
        "updated": updated,
        "dropped": dropped,
        "skipped": skipped,
    });
    println!("[agent] routines: mining done {summary}");
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StepRec;
    use crate::tools::{Source, ToolSpec};

    fn catalog(names: &[&str]) -> Catalog {
        Catalog {
            tools: names
                .iter()
                .map(|n| ToolSpec {
                    name: n.to_string(),
                    description: "d".into(),
                    parameters: json!({"type": "object"}),
                    requires_confirmation: false,
                    risk: "low".into(),
                    timeout_ms: 1000,
                    cooldown_ms: 0,
                    max_per_goal: 16,
                    source: Source::Minimal,
                })
                .collect(),
            allowed_actions: vec![],
            tools_file_set: false,
        }
    }

    fn goal(text: &str, steps: &[(&str, &str, Value)], at: i64) -> MiningRow {
        MiningRow {
            goal: text.into(),
            status: STATUS_COMPLETED.into(),
            steps: steps
                .iter()
                .enumerate()
                .map(|(i, (kind, tool, params))| StepRec {
                    n: i as u32 + 1,
                    kind: kind.to_string(),
                    detail: json!({"tool": tool, "params": params}),
                })
                .collect(),
            updated_at_ms: at,
        }
    }

    fn allow() -> Vec<String> {
        vec!["light".to_string()]
    }

    fn on() -> Value {
        json!({"pin": "D1", "value": 1})
    }

    #[test]
    fn two_identical_successes_make_one_candidate() {
        let goals = [
            goal("включи свет", &[("tool_ok", "light", on())], 1),
            goal(
                "Включи свет.",
                &[("tool_ok", "light", json!({"value": 1, "pin": "D1"}))],
                2,
            ),
        ];
        let got = candidates(&goals, &catalog(&["light"]), &allow(), &Book::default());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].texts, vec!["включи свет"]);
        assert_eq!(got[0].successes, 2);
    }

    #[test]
    fn single_success_foreign_tool_and_errors_do_not_qualify() {
        let cat = catalog(&["light", "tg_send_message"]);
        let once = [goal("включи свет", &[("tool_ok", "light", on())], 1)];
        assert!(candidates(&once, &cat, &allow(), &Book::default()).is_empty());
        let foreign = [
            goal("напиши", &[("tool_ok", "tg_send_message", json!({}))], 1),
            goal("напиши", &[("tool_ok", "tg_send_message", json!({}))], 2),
        ];
        assert!(candidates(&foreign, &cat, &allow(), &Book::default()).is_empty());
        let with_error = [
            goal(
                "включи свет",
                &[("tool_error", "light", on()), ("tool_ok", "light", on())],
                1,
            ),
            goal("включи свет", &[("tool_ok", "light", on())], 2),
        ];
        assert!(candidates(&with_error, &cat, &allow(), &Book::default()).is_empty());
    }

    #[test]
    fn same_words_for_two_different_calls_are_dropped_from_both() {
        let off = json!({"pin": "D1", "value": 0});
        let goals = [
            goal("свет", &[("tool_ok", "light", on())], 1),
            goal("свет", &[("tool_ok", "light", on())], 2),
            goal("свет", &[("tool_ok", "light", off.clone())], 3),
            goal("свет", &[("tool_ok", "light", off)], 4),
        ];
        assert!(candidates(&goals, &catalog(&["light"]), &allow(), &Book::default()).is_empty());
    }

    #[test]
    fn disabled_routine_is_not_relearned_from_older_goals() {
        let goals = [
            goal("включи свет", &[("tool_ok", "light", on())], 1),
            goal("включи свет", &[("tool_ok", "light", on())], 2),
        ];
        let mut book = Book::default();
        let c = candidates(&goals, &catalog(&["light"]), &allow(), &book).remove(0);
        let e = Enrichment {
            learn: true,
            reason: String::new(),
            paraphrases: vec![],
            replies: vec!["Свет включён.".into()],
        };
        merge(&mut book, &c, &e, "h", 5);
        book.disable(0, "user_correction", 10);
        assert!(candidates(&goals, &catalog(&["light"]), &allow(), &book).is_empty());
    }

    #[test]
    fn read_calls_are_never_learned_and_off_calls_lose_on_words() {
        let cat = catalog(&["light"]);
        let state = json!({"command": "get_state", "params": {"name": "led"}});
        let reads = [
            goal(
                "статус светодиода",
                &[("tool_ok", "light", state.clone())],
                1,
            ),
            goal("статус светодиода", &[("tool_ok", "light", state)], 2),
        ];
        assert!(candidates(&reads, &cat, &allow(), &Book::default()).is_empty());

        let off = json!({"command": "set_pin", "params": {"name": "led", "value": 0}});
        let goals = [
            goal("выключи его", &[("tool_ok", "light", off.clone())], 1),
            goal("выключи светодиод", &[("tool_ok", "light", off)], 2),
        ];
        let c = candidates(&goals, &cat, &allow(), &Book::default()).remove(0);
        assert_eq!(c.texts, vec!["выключи светодиод"]);
        let e = Enrichment {
            learn: true,
            reason: String::new(),
            paraphrases: vec!["зажги светодиод".into(), "погаси светодиод".into()],
            replies: vec!["Зажёг.".into(), "Выключил.".into()],
        };
        let mut book = Book::default();
        assert_eq!(merge(&mut book, &c, &e, "h", 1), MergeOutcome::Created);
        assert_eq!(
            book.routines[0].phrases,
            vec!["выключи светодиод", "погаси светодиод"]
        );
        // "Зажёг." is gone, and the one survivor is topped up without a duplicate
        assert_eq!(
            book.routines[0].replies,
            guard::stock_replies(Some(Polarity::Off))
        );
    }

    #[test]
    fn enrichment_parsing_is_lenient_but_filters() {
        let long = "я".repeat(200);
        let raw = format!(
            "```json\n{{\"learn\": true, \"paraphrases\": [\"Свет вкл!\", \"\", {p}], \
             \"replies\": [\"Готово.\", \"{long}\", \" \"]}}\n```",
            p = (0..10)
                .map(|i| format!("\"вариант {i}\""))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let e = parse_enrichment(&raw).unwrap();
        assert!(e.learn);
        assert!(
            !parse_enrichment(r#"{"replies": ["Готово."]}"#)
                .unwrap()
                .learn,
            "missing verdict = no"
        );
        assert_eq!(e.replies, vec!["Готово."]);
        assert_eq!(e.paraphrases.len(), MAX_PARAPHRASES);
        assert_eq!(e.paraphrases[0], "свет вкл");
        assert!(parse_enrichment("Sure! Here you go.").is_none());
        assert!(parse_enrichment(r#"{"replies": []}"#).is_none());
    }

    #[test]
    fn merge_tops_up_replies_and_skips_phrases_owned_elsewhere() {
        let c = Candidate {
            tool: "light".into(),
            params: on(),
            texts: vec!["включи свет".into()],
            successes: 2,
        };
        let declined = Enrichment {
            learn: false,
            reason: "ambiguous".into(),
            paraphrases: vec![],
            replies: vec!["Включил.".into()],
        };
        assert_eq!(
            merge(&mut Book::default(), &c, &declined, "h", 1),
            MergeOutcome::Dropped
        );
        let wrong = Enrichment {
            learn: true,
            reason: String::new(),
            paraphrases: vec![],
            replies: vec!["Выключил.".into()],
        };
        let mut scratch = Book::default();
        assert_eq!(
            merge(&mut scratch, &c, &wrong, "h", 1),
            MergeOutcome::Created
        );
        assert_eq!(
            scratch.routines[0].replies,
            guard::stock_replies(Some(Polarity::On))
        );

        let mut book = Book::default();

        let e = Enrichment {
            learn: true,
            reason: String::new(),
            paraphrases: vec!["зажги свет".into()],
            replies: vec!["Свет включён.".into()],
        };
        assert_eq!(merge(&mut book, &c, &e, "h", 1), MergeOutcome::Created);
        let other = Candidate {
            tool: "light".into(),
            params: json!({"pin": "D2"}),
            texts: vec!["включи второй свет".into()],
            successes: 2,
        };
        assert_eq!(merge(&mut book, &other, &e, "h", 2), MergeOutcome::Created);
        assert_eq!(
            book.routines[1].phrases,
            vec!["включи второй свет"],
            "\"зажги свет\" belongs to r0"
        );
        assert_eq!(merge(&mut book, &c, &e, "h2", 3), MergeOutcome::Updated);
        assert_eq!(book.routines[0].spec_hash, "h2");
    }
}
