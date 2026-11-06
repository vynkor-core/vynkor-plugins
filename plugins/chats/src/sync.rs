use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::store::{session_from_row, turn_from_row, SESSION_COLS, TURN_COLS};

/// One change before it is rendered: ordering key plus JSON body.
struct Change {
    rev: i64,
    /// 0 session, 1 turn, 2 delete — the tie-break inside one revision.
    op_order: u8,
    /// Position inside (rev, op): turns by `seq`.
    sub: i64,
    body: Value,
}

fn db_err(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

/// Changes with `lo < rev <= hi`, each source ordered by rev.
/// `limit` of -1 means unlimited (SQLite semantics).
fn fetch(conn: &Connection, lo: i64, hi: i64, limit: i64) -> Result<Vec<Change>, String> {
    let mut out = Vec::new();

    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {SESSION_COLS}, rev FROM sessions
             WHERE state = 'live' AND rev > ?1 AND rev <= ?2 ORDER BY rev, id LIMIT ?3"
        ))
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![lo, hi, limit], |r| {
            Ok((r.get::<_, i64>(12)?, session_from_row(r)?))
        })
        .map_err(db_err)?;
    for row in rows {
        let (rev, session) = row.map_err(db_err)?;
        out.push(Change {
            rev,
            op_order: 0,
            sub: 0,
            body: json!({"rev": rev, "op": "session", "session": session}),
        });
    }

    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {TURN_COLS}, session_id, rev FROM turns
             WHERE rev > ?1 AND rev <= ?2
               AND session_id IN (SELECT id FROM sessions WHERE state = 'live')
             ORDER BY rev, rowid LIMIT ?3"
        ))
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![lo, hi, limit], |r| {
            let mut turn = turn_from_row(r)?;
            turn["session_id"] = json!(r.get::<_, String>(8)?);
            Ok((r.get::<_, i64>(9)?, r.get::<_, i64>(1)?, turn))
        })
        .map_err(db_err)?;
    for row in rows {
        let (rev, seq, turn) = row.map_err(db_err)?;
        out.push(Change {
            rev,
            op_order: 1,
            sub: seq,
            body: json!({"rev": rev, "op": "turn", "turn": turn}),
        });
    }

    let mut stmt = conn
        .prepare_cached(
            "SELECT id, rev FROM tombstones WHERE rev > ?1 AND rev <= ?2 ORDER BY rev, id LIMIT ?3",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![lo, hi, limit], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(db_err)?;
    for row in rows {
        let (id, rev) = row.map_err(db_err)?;
        out.push(Change {
            rev,
            op_order: 2,
            sub: 0,
            body: json!({"rev": rev, "op": "delete", "id": id}),
        });
    }

    out.sort_by_key(|c| (c.rev, c.op_order, c.sub));
    Ok(out)
}

/// Changes with `rev > since_rev`, oldest first. At most `min(limit, sync_max)`
/// changes are taken, then the page is extended to include every change that
/// shares the last revision, so a revision group is never split across pages.
pub fn sync(
    conn: &Connection,
    since_rev: i64,
    limit: usize,
    sync_max: usize,
) -> Result<Value, String> {
    let limit = limit.min(sync_max).max(1);
    let review_pending: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM reviews WHERE status = 'pending')",
            [],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    let current: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = 'rev'", [], |r| r.get(0))
        .map_err(db_err)?;
    if since_rev > current {
        // The caller is ahead of this store (restored/replaced database):
        // it must drop its local copy and resync from 0.
        return Ok(
            json!({"changes": [], "rev": current, "more": false, "review_pending": review_pending, "reset": true}),
        );
    }
    let mut page = fetch(conn, since_rev, i64::MAX, limit as i64)?;
    page.truncate(limit);
    let last_rev = match page.last() {
        None => {
            return Ok(
                json!({"changes": [], "rev": since_rev, "more": false, "review_pending": review_pending, "reset": false}),
            );
        }
        Some(c) => c.rev,
    };
    // Complete the last group: re-read every change at exactly `last_rev`.
    page.retain(|c| c.rev < last_rev);
    page.extend(fetch(conn, last_rev - 1, last_rev, -1)?);
    let more = !fetch(conn, last_rev, i64::MAX, 1)?.is_empty();
    let changes: Vec<Value> = page.into_iter().map(|c| c.body).collect();
    Ok(
        json!({"changes": changes, "rev": last_rev, "more": more, "review_pending": review_pending, "reset": false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::schema::open_temp;
    use crate::store::{
        append, delete, resolve,
        tests::{ap, rp, sid},
        update,
    };
    use serde_json::{json, Value};
    use std::collections::HashSet;

    fn changes(v: &Value) -> &Vec<Value> {
        v["changes"].as_array().unwrap()
    }

    #[test]
    fn sync_from_zero_returns_everything_in_rev_order() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let a = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), 100).unwrap());
        let b = sid(&resolve(&mut c, &cfg, &rp("phone", "chat", None), 100).unwrap());
        append(&mut c, &cfg, ap(&a, "q1", "a1"), 101).unwrap();
        append(&mut c, &cfg, ap(&b, "q2", "a2"), 102).unwrap();
        update(&mut c, &a, Some("T".into()), None).unwrap();
        let del = delete(&mut c, &b).unwrap();
        let r = sync(&c, 0, 500, 500).unwrap();
        let ch = changes(&r);
        let revs: Vec<i64> = ch.iter().map(|e| e["rev"].as_i64().unwrap()).collect();
        assert!(
            revs.windows(2).all(|w| w[0] <= w[1]),
            "not sorted: {revs:?}"
        );
        // live session a (latest rev = title update), its turn, tombstone of b
        let ops: Vec<(&str, i64)> = ch
            .iter()
            .map(|e| (e["op"].as_str().unwrap(), e["rev"].as_i64().unwrap()))
            .collect();
        assert_eq!(ops.len(), 3, "{ops:?}");
        assert_eq!(ops[0].0, "turn");
        assert_eq!(ops[1], ("session", ops[1].1));
        assert_eq!(ops[2], ("delete", del["rev"].as_i64().unwrap()));
        assert_eq!(ch[0]["turn"]["session_id"], a.as_str());
        assert_eq!(ch[0]["turn"]["user_text"], "q1");
        assert_eq!(ch[1]["session"]["title"], "T");
        assert_eq!(ch[2]["id"], b.as_str());
        assert_eq!(r["rev"], del["rev"]);
        assert_eq!(r["more"], false);
        assert_eq!(r["review_pending"], false);
    }

    #[test]
    fn sync_pages_cover_all_changes_once() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), 100).unwrap());
        let mut expect = HashSet::new();
        for i in 0..300 {
            let t = append(&mut c, &cfg, ap(&s, &format!("q{i}"), "a"), 101 + i).unwrap();
            expect.insert(format!("turn:{}", t["turn_id"].as_str().unwrap()));
        }
        expect.insert(format!("session:{s}"));
        let mut seen = Vec::new();
        let (mut since, mut pages) = (0, 0);
        loop {
            let r = sync(&c, since, 50, 500).unwrap();
            pages += 1;
            for e in changes(&r) {
                assert!(e["rev"].as_i64().unwrap() > since);
                seen.push(match e["op"].as_str().unwrap() {
                    "turn" => format!("turn:{}", e["turn"]["id"].as_str().unwrap()),
                    "session" => format!("session:{}", e["session"]["id"].as_str().unwrap()),
                    other => other.to_string(),
                });
            }
            since = r["rev"].as_i64().unwrap();
            if !r["more"].as_bool().unwrap() {
                break;
            }
            assert!(pages < 100, "paging does not terminate");
        }
        assert!(pages >= 6, "expected several pages, got {pages}");
        let uniq: HashSet<_> = seen.iter().cloned().collect();
        assert_eq!(uniq.len(), seen.len(), "a change was returned twice");
        assert_eq!(uniq, expect);
    }

    #[test]
    fn sync_does_not_split_same_rev_group() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), 100).unwrap());
        let t = append(&mut c, &cfg, ap(&s, "q", "a"), 101).unwrap();
        // session row and turn share the append's rev; limit 1 must return both
        let r = sync(&c, 0, 1, 500).unwrap();
        let ch = changes(&r);
        assert_eq!(ch.len(), 2, "{r}");
        assert_eq!(
            (ch[0]["op"].as_str(), ch[1]["op"].as_str()),
            (Some("session"), Some("turn"))
        );
        assert_eq!(r["rev"], t["rev"]);
        assert_eq!(r["more"], false);
        // with an earlier rev present the groups still stay whole
        let b = sid(&resolve(&mut c, &cfg, &rp("phone", "chat", None), 100).unwrap());
        append(&mut c, &cfg, ap(&b, "q", "a"), 101).unwrap();
        let r1 = sync(&c, 0, 1, 500).unwrap();
        assert_eq!(changes(&r1).len(), 2);
        assert_eq!(r1["more"], true);
        let r2 = sync(&c, r1["rev"].as_i64().unwrap(), 1, 500).unwrap();
        assert_eq!(changes(&r2).len(), 2);
        assert_eq!(r2["more"], false);
        // sync_max caps the requested limit
        let r3 = sync(&c, 0, 1000, 1).unwrap();
        assert_eq!(changes(&r3).len(), 2);
        assert_eq!(r3["more"], true);
    }

    #[test]
    fn sync_empty_returns_since_rev() {
        let (_d, c) = open_temp();
        assert_eq!(
            sync(&c, 0, 500, 500).unwrap(),
            json!({"changes": [], "rev": 0, "more": false, "review_pending": false, "reset": false})
        );
    }

    #[test]
    fn sync_since_ahead_of_store_resets() {
        let (_d, mut c) = open_temp();
        let cfg = test_config();
        let s = sid(&resolve(&mut c, &cfg, &rp("lap", "chat", None), 100).unwrap());
        append(&mut c, &cfg, ap(&s, "q", "a"), 101).unwrap();
        let cur: i64 = c
            .query_row("SELECT value FROM meta WHERE key='rev'", [], |r| r.get(0))
            .unwrap();
        let r = sync(&c, cur + 50, 500, 500).unwrap();
        assert_eq!(r["reset"], true);
        assert_eq!(r["rev"], cur);
        assert_eq!(r["changes"], json!([]));
        assert_eq!(r["more"], false);
        // exactly current is a normal empty page, not a reset
        let r = sync(&c, cur, 500, 500).unwrap();
        assert_eq!(
            (r["reset"].as_bool(), r["rev"].as_i64()),
            (Some(false), Some(cur))
        );
    }
}
