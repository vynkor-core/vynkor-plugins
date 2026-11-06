use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Default, Deserialize)]
pub struct SearchParams {
    pub query: String,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub kind: Option<String>,
    pub limit: Option<i64>,
}

/// Turn free text into an FTS5 expression: every alphanumeric word becomes a
/// quoted prefix term, all ANDed. Quoting makes FTS operators and punctuation
/// in user input inert. `None` when no word is left.
pub fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

pub fn search(conn: &Connection, p: &SearchParams) -> Result<Value, String> {
    let Some(expr) = fts_query(&p.query) else {
        return Ok(json!({"hits": []}));
    };
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let mut stmt = conn
        .prepare_cached(
            "SELECT t.session_id, t.id, t.at,
                    snippet(turns_fts, -1, '[', ']', '…', 12),
                    -bm25(turns_fts)
             FROM turns_fts
             JOIN turns t ON t.rowid = turns_fts.rowid
             JOIN sessions s ON s.id = t.session_id
             WHERE turns_fts MATCH ?1
               AND s.state = 'live'
               AND (?2 IS NULL OR t.at >= ?2)
               AND (?3 IS NULL OR t.at <= ?3)
               AND (?4 IS NULL OR s.kind = ?4)
             ORDER BY bm25(turns_fts), t.at DESC
             LIMIT ?5",
        )
        .map_err(|e| format!("database error: {e}"))?;
    let hits = stmt
        .query_map(params![expr, p.from, p.to, p.kind, limit], |r| {
            Ok(json!({
                "session_id": r.get::<_, String>(0)?,
                "turn_id": r.get::<_, String>(1)?,
                "at": r.get::<_, i64>(2)?,
                "snippet": r.get::<_, String>(3)?,
                "score": r.get::<_, f64>(4)?,
            }))
        })
        .map_err(|e| format!("database error: {e}"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("database error: {e}"))?;
    Ok(json!({"hits": hits}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::schema::open_temp;
    use crate::store::{append, resolve, tests::{ap, rp, sid}};

    fn add(c: &mut Connection, device: &str, kind: &str, at: i64, user: &str, answer: &str) -> (String, String) {
        let cfg = test_config();
        let mut p = rp(device, kind, None);
        p.source = "t".into();
        let s = sid(&resolve(c, &cfg, &p, at).unwrap());
        let t = append(c, &cfg, ap(&s, user, answer), at).unwrap();
        (s, t["turn_id"].as_str().unwrap().to_string())
    }

    fn sp(q: &str) -> SearchParams {
        SearchParams { query: q.into(), ..Default::default() }
    }

    #[test]
    fn search_finds_word_in_answer() {
        let (_d, mut c) = open_temp();
        let (s, t) = add(&mut c, "lap", "chat", 100, "what is up", "The capital of France is Paris");
        add(&mut c, "lap", "chat", 200, "hello", "unrelated");
        let r = search(&c, &sp("paris")).unwrap();
        let hits = r["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["session_id"], s.as_str());
        assert_eq!(hits[0]["turn_id"], t.as_str());
        assert_eq!(hits[0]["at"], 100);
        assert!(hits[0]["snippet"].as_str().unwrap().contains("[Paris]"));
        assert!(hits[0]["score"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn search_prefix_match() {
        let (_d, mut c) = open_temp();
        add(&mut c, "lap", "voice", 100, "какая погода завтра", "солнечно");
        assert_eq!(search(&c, &sp("погод")).unwrap()["hits"].as_array().unwrap().len(), 1);
        assert_eq!(search(&c, &sp("weat")).unwrap()["hits"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn search_cyrillic_case_insensitive() {
        let (_d, mut c) = open_temp();
        add(&mut c, "lap", "voice", 100, "какая погода завтра", "солнечно");
        assert_eq!(search(&c, &sp("Погода")).unwrap()["hits"].as_array().unwrap().len(), 1);
        assert_eq!(search(&c, &sp("СОЛНЕЧНО")).unwrap()["hits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn search_handles_fts_syntax_chars() {
        let (_d, mut c) = open_temp();
        add(&mut c, "lap", "chat", 100, "the weather or not", "sunny");
        for q in ["\"weather\" OR (", "-*", "weather AND", "NEAR(", "\"", "col:weather", "'; DROP TABLE turns;--"] {
            let r = search(&c, &sp(q));
            assert!(r.is_ok(), "query {q:?} failed: {r:?}");
        }
        assert_eq!(search(&c, &sp("\"weather\" OR (")).unwrap()["hits"].as_array().unwrap().len(), 1);
        assert_eq!(search(&c, &sp("-*")).unwrap(), json!({"hits": []}));
        assert_eq!(search(&c, &sp("   ")).unwrap(), json!({"hits": []}));
    }

    #[test]
    fn search_filters_by_time_and_kind() {
        let (_d, mut c) = open_temp();
        add(&mut c, "lap", "chat", 100, "cats", "x");
        add(&mut c, "lap", "voice", 200, "cats", "x");
        add(&mut c, "phone", "chat", 300, "cats", "x");
        let n = |p: SearchParams| search(&c, &p).unwrap()["hits"].as_array().unwrap().len();
        assert_eq!(n(sp("cats")), 3);
        assert_eq!(n(SearchParams { kind: Some("voice".into()), ..sp("cats") }), 1);
        assert_eq!(n(SearchParams { from: Some(150), ..sp("cats") }), 2);
        assert_eq!(n(SearchParams { from: Some(150), to: Some(250), ..sp("cats") }), 1);
        assert_eq!(n(SearchParams { limit: Some(2), ..sp("cats") }), 2);
        let r = search(&c, &sp("cats")).unwrap();
        assert_eq!(r["hits"][0]["at"].as_i64().is_some(), true);
    }
}
