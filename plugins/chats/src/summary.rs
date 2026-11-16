//! Background session summaries: pick idle sessions, ask `ai` for a JSON
//! title/summary/tags, write them back under the session's liveness check.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use crate::db::with_db;
use crate::store::next_rev;
use crate::worker::Worker;

pub const PROMPT: &str = "Summarize this conversation for later search. Reply with JSON only: {\"title\": \"<= 8 words\", \"summary\": \"<= 3 sentences\", \"tags\": [\"<= 5 short lowercase tags\"]}. Write in the language of the conversation.";

/// Sessions summarized per worker pass.
const BATCH: usize = 5;
const TRANSCRIPT_MAX_CHARS: usize = 12_000;
/// A session whose summary call failed is not retried for this long.
const SESSION_RETRY_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub title: String,
    pub summary: String,
    pub tags: Vec<String>,
}

/// Parse the model reply: a bare JSON object, optionally in a ```json fence
/// or surrounded by prose (the first `{` .. last `}` is taken). An empty
/// `summary` is not a summary.
pub fn parse_summary(content: &str) -> Option<Summary> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end < start {
        return None;
    }
    let v: Value = serde_json::from_str(&content[start..=end]).ok()?;
    let summary = v.get("summary")?.as_str()?.trim().to_string();
    if summary.is_empty() {
        return None;
    }
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let tags = v
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .take(5)
                .collect()
        })
        .unwrap_or_default();
    Some(Summary {
        title,
        summary,
        tags,
    })
}

/// "User: …\nAssistant: …" blocks, cut to [`TRANSCRIPT_MAX_CHARS`] keeping
/// the beginning and the end.
pub fn build_transcript(turns: &[(String, String)]) -> String {
    let full = turns
        .iter()
        .map(|(u, a)| format!("User: {u}\nAssistant: {a}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let chars: Vec<char> = full.chars().collect();
    if chars.len() <= TRANSCRIPT_MAX_CHARS {
        return full;
    }
    let half = TRANSCRIPT_MAX_CHARS / 2;
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    format!("{head}\n[...]\n{tail}")
}

fn db_err(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

struct Candidate {
    id: String,
    rev: i64,
    transcript: String,
}

fn candidates(
    conn: &Connection,
    idle_before: i64,
    skip: &std::collections::HashSet<String>,
) -> Result<Vec<Candidate>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, rev FROM sessions
             WHERE state = 'live' AND turns > 0 AND last_at <= ?1 AND summarized_rev < rev
             ORDER BY last_at LIMIT ?2",
        )
        .map_err(db_err)?;
    let rows: Vec<(String, i64)> = stmt
        .query_map(params![idle_before, (BATCH + skip.len()) as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err)?;
    let rows: Vec<(String, i64)> = rows
        .into_iter()
        .filter(|(id, _)| !skip.contains(id))
        .take(BATCH)
        .collect();
    let mut out = Vec::new();
    for (id, rev) in rows {
        let turns: Vec<(String, String)> = conn
            .prepare(
                "SELECT user_text, answer_text FROM turns
                 WHERE session_id = ? AND status = 'ok' ORDER BY seq",
            )
            .map_err(db_err)?
            .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_err)?;
        out.push(Candidate {
            id,
            rev,
            transcript: build_transcript(&turns),
        });
    }
    Ok(out)
}

/// Write a result for `id`. `seen_rev` is the revision the transcript was
/// read at. Returns true if synced state changed (a new rev was issued).
///
/// - session no longer live: nothing is written;
/// - `None` (garbage / `ai` failure): only `summarized_rev = seen_rev`, so
///   the session is retried when a new turn bumps its rev;
/// - `Some`: summary/tags/title (title unless set manually) and a new rev;
///   `summarized_rev` follows the new rev only if no turn arrived meanwhile.
pub fn apply(
    conn: &mut Connection,
    id: &str,
    seen_rev: i64,
    s: Option<&Summary>,
) -> Result<bool, String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let cur: Option<i64> = tx
        .query_row(
            "SELECT rev FROM sessions WHERE id = ? AND state = 'live'",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let Some(cur) = cur else {
        return Ok(false);
    };
    let changed = match s {
        None => {
            tx.execute(
                "UPDATE sessions SET
                    embedded_rev = CASE WHEN embedded_rev >= summarized_rev
                                        THEN MAX(embedded_rev, ?1) ELSE embedded_rev END,
                    summarized_rev = MAX(summarized_rev, ?1)
                 WHERE id = ?2",
                params![seen_rev, id],
            )
            .map_err(db_err)?;
            false
        }
        Some(s) => {
            let rev = next_rev(&tx).map_err(db_err)?;
            let marked = if cur == seen_rev { rev } else { seen_rev };
            tx.execute(
                "UPDATE sessions SET summary = ?1, tags = ?2,
                    title = CASE WHEN title_manual = 0 AND ?3 != '' THEN ?3 ELSE title END,
                    rev = ?4, summarized_rev = ?5
                 WHERE id = ?6",
                params![
                    s.summary,
                    json!(s.tags).to_string(),
                    s.title,
                    rev,
                    marked,
                    id
                ],
            )
            .map_err(db_err)?;
            true
        }
    };
    tx.commit().map_err(db_err)?;
    Ok(changed)
}

impl Worker {
    /// Summarize up to [`BATCH`] idle sessions. Returns true if anything
    /// synced changed.
    pub(crate) async fn summarize_pass(&self, now: i64) -> bool {
        let idle_before = now - self.cfg.summary_idle_secs.saturating_mul(1000);
        let skip: std::collections::HashSet<String> = {
            let mut m = self.summary_retry_at.lock().unwrap();
            m.retain(|_, until| *until > now);
            m.keys().cloned().collect()
        };
        let cands = match with_db(&self.db, move |c| candidates(c, idle_before, &skip)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) | Err(e) => {
                self.warn("db", &e);
                return false;
            }
        };
        let mut changed = false;
        let mut consecutive_errors = 0;
        for cand in cands {
            let mut params = json!({
                "messages": [{"role": "user", "content": format!("{PROMPT}\n\n{}", cand.transcript)}],
                "max_tokens": self.cfg.summary_max_tokens,
            });
            if let Some(a) = &self.cfg.ai_agent_id {
                params["agent_id"] = json!(a);
            }
            let reply = self.rpc.call("chat_completion", params, 60_000).await;
            let parsed = match reply {
                Ok(v) => {
                    self.ok("ai");
                    consecutive_errors = 0;
                    let p = v["content"].as_str().and_then(parse_summary);
                    if p.is_none() {
                        self.warn("ai-reply", "ai reply was not a usable summary");
                    }
                    p
                }
                Err(e) => {
                    // Timeout, 429, 5xx, plugin missing: nothing is recorded, the
                    // session stays a candidate. Only an unusable *reply* counts
                    // as processed. Back off this session and give up on the
                    // pass once `ai` fails twice in a row.
                    self.warn("ai", &e);
                    self.summary_retry_at
                        .lock()
                        .unwrap()
                        .insert(cand.id.clone(), now + SESSION_RETRY_MS);
                    consecutive_errors += 1;
                    if consecutive_errors >= 2 || crate::rpc::is_missing_plugin(&e) {
                        return changed;
                    }
                    continue;
                }
            };
            let id = cand.id.clone();
            let rev = cand.rev;
            match with_db(&self.db, move |c| apply(c, &id, rev, parsed.as_ref())).await {
                Ok(Ok(true)) => changed = true,
                Ok(Ok(false)) => {}
                Ok(Err(e)) | Err(e) => {
                    self.warn("db", &e);
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync;
    use crate::testkit::{Fixture, T0};

    const REPLY: &str = r#"{"title":"Weather in Tashkent","summary":"User asked about the weather; it is sunny.","tags":["Weather","Tashkent"]}"#;

    #[test]
    fn parse_summary_accepts_fenced_json() {
        let s = parse_summary(&format!("Sure!\n```json\n{REPLY}\n```\nDone")).unwrap();
        assert_eq!(s.title, "Weather in Tashkent");
        assert_eq!(s.tags, vec!["weather", "tashkent"]);
        assert!(parse_summary(REPLY).is_some());
    }

    #[test]
    fn parse_summary_rejects_empty() {
        assert!(parse_summary(r#"{"title":"x","summary":"  ","tags":[]}"#).is_none());
        assert!(parse_summary(r#"{"title":"x"}"#).is_none());
        assert!(parse_summary("no json here").is_none());
        assert!(parse_summary("} {").is_none());
    }

    #[test]
    fn transcript_keeps_head_and_tail() {
        let big = "x".repeat(20_000);
        let t = build_transcript(&[("START".into(), big.clone()), (big, "END".into())]);
        assert!(t.chars().count() < 12_100);
        assert!(t.starts_with("User: START") && t.ends_with("END"));
    }

    fn setup(f: &Fixture) -> (String, i64) {
        let sid = f.session("lap", "chat", T0);
        f.turn(&sid, "what is the weather", "sunny", T0);
        (sid, T0 + 1_000_000) // well past the 900 s idle window
    }

    #[tokio::test]
    async fn idle_session_gets_summary_and_rev_bumps() {
        let f = Fixture::new();
        let (sid, later) = setup(&f);
        let rev_before: i64 = f.q("SELECT rev FROM sessions WHERE id=?", [&sid]);
        f.fake.script_completion(Ok(REPLY));
        f.worker.run_once(later).await;
        let (title, summary, tags, rev, sum_rev): (String, String, String, i64, i64) = f.q2(
            "SELECT title, summary, tags, rev, summarized_rev FROM sessions WHERE id=?",
            &sid,
        );
        assert_eq!(title, "Weather in Tashkent");
        assert!(summary.contains("sunny"));
        assert_eq!(tags, r#"["weather","tashkent"]"#);
        assert!(rev > rev_before);
        assert_eq!(sum_rev, rev);
        // visible to sync clients
        let out = sync::sync(&f.lock(), rev_before, 100, 500).unwrap();
        assert_eq!(out["changes"][0]["session"]["title"], "Weather in Tashkent");
        // request carries the exact prompt, the transcript and the token cap
        let call = &f.fake.calls("chat_completion")[0];
        let content = call["messages"][0]["content"].as_str().unwrap();
        assert!(content.starts_with(PROMPT) && content.contains("User: what is the weather"));
        assert_eq!(call["max_tokens"], 400);
        // second pass: nothing to do
        f.worker.run_once(later).await;
        assert_eq!(f.fake.calls("chat_completion").len(), 1);
    }

    #[tokio::test]
    async fn fresh_session_is_not_summarized_yet() {
        let f = Fixture::new();
        let (_sid, _) = setup(&f);
        f.fake.script_completion(Ok(REPLY));
        f.worker.run_once(T0 + 60_000).await;
        assert_eq!(f.fake.calls("chat_completion").len(), 0);
    }

    #[tokio::test]
    async fn manual_title_is_kept() {
        let f = Fixture::new();
        let (sid, later) = setup(&f);
        {
            let mut c = f.lock();
            crate::store::update(&mut c, &sid, Some("My own title".into()), None).unwrap();
        }
        f.fake.script_completion(Ok(REPLY));
        f.worker.run_once(later).await;
        let (title, summary): (String, String) =
            f.q2("SELECT title, summary FROM sessions WHERE id=?", &sid);
        assert_eq!(title, "My own title");
        assert!(summary.contains("sunny"));
    }

    #[tokio::test]
    async fn garbage_reply_marks_processed_without_overwrite() {
        let f = Fixture::new();
        let (sid, later) = setup(&f);
        {
            let c = f.lock();
            c.execute(
                "UPDATE sessions SET summary='old', title='Old' WHERE id=?",
                [&sid],
            )
            .unwrap();
        }
        f.fake.script_completion(Ok("I cannot do that"));
        f.worker.run_once(later).await;
        let (title, summary, rev, sum_rev): (String, String, i64, i64) = f.q2(
            "SELECT title, summary, rev, summarized_rev FROM sessions WHERE id=?",
            &sid,
        );
        assert_eq!((title.as_str(), summary.as_str()), ("Old", "old"));
        assert_eq!(sum_rev, rev);
        f.worker.run_once(later).await;
        assert_eq!(
            f.fake.calls("chat_completion").len(),
            1,
            "no endless retries"
        );
        // a new turn makes it a candidate again
        f.turn(&sid, "more", "stuff", T0 + 10);
        f.fake.script_completion(Ok(REPLY));
        f.worker.run_once(later).await;
        assert_eq!(f.fake.calls("chat_completion").len(), 2);
    }

    #[tokio::test]
    async fn deleted_during_ai_call_writes_nothing() {
        let f = Fixture::new();
        let (sid, later) = setup(&f);
        f.fake.script_completion(Ok(REPLY));
        // keep the row around after the cascade tried (vector-db down)
        f.fake.set_failing("vector-db", true);
        let db = std::sync::Arc::clone(&f.db);
        let sid2 = sid.clone();
        f.fake.on_call("chat_completion", move |_| {
            let mut c = db.lock().unwrap();
            crate::store::delete(&mut c, &sid2).unwrap();
        });
        f.worker.run_once(later).await;
        let (title, summary, state): (String, String, String) = f.q2(
            "SELECT title, summary, state FROM sessions WHERE id=?",
            &sid,
        );
        assert_eq!(
            (title.as_str(), summary.as_str(), state.as_str()),
            ("", "", "deleting")
        );
    }

    #[tokio::test]
    async fn turn_during_ai_call_keeps_session_pending() {
        let f = Fixture::new();
        let (sid, later) = setup(&f);
        f.fake.script_completion(Ok(REPLY));
        let (db, cfg, sid2) = (std::sync::Arc::clone(&f.db), f.cfg.clone(), sid.clone());
        f.fake.on_call("chat_completion", move |_| {
            let mut c = db.lock().unwrap();
            crate::store::append(
                &mut c,
                &cfg,
                crate::store::tests::ap(&sid2, "and tomorrow", "rain"),
                T0 + 5,
            )
            .unwrap();
        });
        f.worker.run_once(later).await;
        let (rev, sum_rev): (i64, i64) =
            f.q2("SELECT rev, summarized_rev FROM sessions WHERE id=?", &sid);
        assert!(sum_rev < rev, "new turn must trigger another summary");
    }

    #[tokio::test]
    async fn ai_transport_error_is_retried_not_marked_processed() {
        let f = Fixture::new();
        let (sid, later) = setup(&f);
        f.fake.script_completion(Err("429 rate limited"));
        f.worker.run_once(later).await;
        let (rev, sum_rev, summary): (i64, i64, String) = f.q2(
            "SELECT rev, summarized_rev, summary FROM sessions WHERE id=?",
            &sid,
        );
        assert_eq!(
            (sum_rev, summary.as_str()),
            (0, ""),
            "rev {rev} must stay pending"
        );
        // next tick (after the short per-session backoff) summarizes it
        f.fake.script_completion(Ok(REPLY));
        f.worker.run_once(later + SESSION_RETRY_MS + 1).await;
        let (title, sum_rev, rev): (String, i64, i64) = f.q2(
            "SELECT title, summarized_rev, rev FROM sessions WHERE id=?",
            &sid,
        );
        assert_eq!(title, "Weather in Tashkent");
        assert_eq!(sum_rev, rev);
    }

    #[tokio::test]
    async fn failing_session_does_not_starve_others() {
        let f = Fixture::new();
        let (a, later) = setup(&f);
        let b = f.session("pc", "chat", T0);
        f.turn(&b, "second", "chat", T0 + 1);
        f.fake.script_completion(Err("500"));
        f.fake.script_completion(Ok(REPLY));
        f.worker.run_once(later).await;
        let sa: i64 = f.q("SELECT summarized_rev FROM sessions WHERE id=?", [&a]);
        let sb: i64 = f.q("SELECT summarized_rev FROM sessions WHERE id=?", [&b]);
        assert_eq!(sa, 0);
        assert!(sb > 0, "the next session is still summarized");
    }

    #[tokio::test]
    async fn missing_ai_is_skipped_quietly() {
        let f = Fixture::new();
        let (sid, later) = setup(&f);
        f.fake.set_missing("ai", true);
        f.worker.run_once(later).await;
        let sum_rev: i64 = f.q("SELECT summarized_rev FROM sessions WHERE id=?", [&sid]);
        assert_eq!(sum_rev, 0, "retried once ai appears");
    }
}
