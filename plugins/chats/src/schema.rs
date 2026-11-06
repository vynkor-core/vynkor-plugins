use std::path::Path;

use rusqlite::Connection;

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS sessions(
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  device TEXT NOT NULL,
  source TEXT NOT NULL DEFAULT '',
  title TEXT NOT NULL DEFAULT '',
  started_at INTEGER NOT NULL,
  last_at INTEGER NOT NULL,
  summary TEXT NOT NULL DEFAULT '',
  tags TEXT NOT NULL DEFAULT '[]',
  tier TEXT NOT NULL DEFAULT 'full',
  pinned INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL DEFAULT 'live',   -- live | deleting
  turns INTEGER NOT NULL DEFAULT 0,
  rev INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_dev_kind_last ON sessions(device, kind, last_at);
CREATE INDEX IF NOT EXISTS sessions_last ON sessions(last_at);
CREATE INDEX IF NOT EXISTS sessions_rev ON sessions(rev);

CREATE TABLE IF NOT EXISTS turns(
  rowid INTEGER PRIMARY KEY,
  id TEXT NOT NULL UNIQUE,
  session_id TEXT NOT NULL REFERENCES sessions(id),
  seq INTEGER NOT NULL,
  at INTEGER NOT NULL,
  user_text TEXT NOT NULL DEFAULT '',
  answer_text TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL,
  tool_trace TEXT NOT NULL DEFAULT '[]',
  goal_id TEXT,
  rev INTEGER NOT NULL,
  UNIQUE(session_id, seq)
);
CREATE INDEX IF NOT EXISTS turns_rev ON turns(rev);

CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
  user_text, answer_text,
  content='turns', content_rowid='rowid',
  tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS turns_ai AFTER INSERT ON turns BEGIN
  INSERT INTO turns_fts(rowid, user_text, answer_text) VALUES (new.rowid, new.user_text, new.answer_text);
END;
CREATE TRIGGER IF NOT EXISTS turns_ad AFTER DELETE ON turns BEGIN
  INSERT INTO turns_fts(turns_fts, rowid, user_text, answer_text) VALUES ('delete', old.rowid, old.user_text, old.answer_text);
END;
CREATE TRIGGER IF NOT EXISTS turns_au AFTER UPDATE ON turns BEGIN
  INSERT INTO turns_fts(turns_fts, rowid, user_text, answer_text) VALUES ('delete', old.rowid, old.user_text, old.answer_text);
  INSERT INTO turns_fts(rowid, user_text, answer_text) VALUES (new.rowid, new.user_text, new.answer_text);
END;

CREATE TABLE IF NOT EXISTS tombstones(id TEXT PRIMARY KEY, rev INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS tombstones_rev ON tombstones(rev);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT OR IGNORE INTO meta(key, value) VALUES ('rev', 0);
";

pub const USER_VERSION: i64 = 1;

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if v > USER_VERSION {
        return Err(rusqlite::Error::ToSqlConversionFailure(
            format!("chats.db has schema v{v}, this build understands v{USER_VERSION}").into(),
        ));
    }
    conn.execute_batch(SCHEMA_V1)?;
    conn.pragma_update(None, "user_version", USER_VERSION)?;
    Ok(conn)
}

#[cfg(test)]
pub(crate) fn open_temp() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(&dir.path().join("chats.db")).unwrap();
    (dir, conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts5_available() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE VIRTUAL TABLE t USING fts5(a)")
            .unwrap();
    }

    #[test]
    fn open_creates_schema_v1() {
        let (_d, c) = open_temp();
        let v: i64 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        for t in ["sessions", "turns", "turns_fts", "tombstones", "meta"] {
            let n: i64 = c
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name=?",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {t}");
        }
        let rev: i64 = c
            .query_row("SELECT value FROM meta WHERE key='rev'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rev, 0);
        let fk: i64 = c
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        let jm: String = c
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(jm, "wal");
    }

    #[test]
    fn open_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chats.db");
        let c = open(&p).unwrap();
        c.execute("UPDATE meta SET value=7 WHERE key='rev'", [])
            .unwrap();
        drop(c);
        let c = open(&p).unwrap();
        let rev: i64 = c
            .query_row("SELECT value FROM meta WHERE key='rev'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rev, 7);
    }
}
