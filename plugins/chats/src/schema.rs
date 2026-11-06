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

/// v2 (semantic layer): worker bookkeeping columns plus the delete-cascade
/// queue (`cascade`) and the late-vector sweep list (`swept`).
const SCHEMA_V2_COLUMNS: [(&str, &str, &str); 5] = [
    ("sessions", "summarized_rev", "INTEGER NOT NULL DEFAULT 0"),
    ("sessions", "title_manual", "INTEGER NOT NULL DEFAULT 0"),
    ("sessions", "embedded_rev", "INTEGER NOT NULL DEFAULT 0"),
    // 0 = pending, 1 = done, -1 = given up after repeated rejections
    ("turns", "embedded", "INTEGER NOT NULL DEFAULT 0"),
    ("turns", "embed_attempts", "INTEGER NOT NULL DEFAULT 0"),
];

const SCHEMA_V2_TABLES: &str = "
CREATE TABLE IF NOT EXISTS cascade(
  session_id TEXT PRIMARY KEY,
  turn_ids TEXT NOT NULL DEFAULT '[]',
  goal_ids TEXT NOT NULL DEFAULT '[]',
  attempts INTEGER NOT NULL DEFAULT 0,
  next_at INTEGER NOT NULL DEFAULT 0,
  done TEXT NOT NULL DEFAULT '[]',
  -- bumped by every requeue; a job is only finished/updated at the generation it read
  gen INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS swept(
  session_id TEXT PRIMARY KEY,
  turn_ids TEXT NOT NULL DEFAULT '[]',
  until INTEGER NOT NULL,
  -- sweep cycle (start time) in which this row was last swept
  swept_at INTEGER NOT NULL DEFAULT 0,
  -- per-row retry state: failing rows go to the back of the queue
  failures INTEGER NOT NULL DEFAULT 0,
  next_at INTEGER NOT NULL DEFAULT 0
);
";
const SCHEMA_V2_INDEXES: &str =
    "CREATE INDEX IF NOT EXISTS turns_embedded ON turns(embedded, session_id);";

/// Columns of the v2 tables that were added after the first v2 shape; applied
/// to databases that already have the tables.
const SCHEMA_V2_TABLE_COLUMNS: [(&str, &str, &str); 4] = [
    ("cascade", "gen", "INTEGER NOT NULL DEFAULT 0"),
    ("swept", "swept_at", "INTEGER NOT NULL DEFAULT 0"),
    ("swept", "failures", "INTEGER NOT NULL DEFAULT 0"),
    ("swept", "next_at", "INTEGER NOT NULL DEFAULT 0"),
];

/// v3 (retention): usage counters, cascade modes and the monthly review.
const SCHEMA_V3_COLUMNS: [(&str, &str, &str); 3] = [
    ("sessions", "recall_count", "INTEGER NOT NULL DEFAULT 0"),
    ("sessions", "last_used_at", "INTEGER NOT NULL DEFAULT 0"),
    // 'delete' = remove everything of the session, 'compact' = vectors of the
    // turns and agent goals only; the session itself stays
    ("cascade", "mode", "TEXT NOT NULL DEFAULT 'delete'"),
];

const SCHEMA_V3_TABLES: &str = "
CREATE TABLE IF NOT EXISTS reviews(
  id TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL,
  status TEXT NOT NULL,            -- pending | applied | dismissed
  groups TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS reviews_status ON reviews(status);
";

fn ensure_v3_shape(conn: &Connection) -> rusqlite::Result<()> {
    add_missing_columns(conn, &SCHEMA_V3_COLUMNS)?;
    conn.execute_batch(SCHEMA_V3_TABLES)
}

pub const USER_VERSION: i64 = 3;

fn has_column(conn: &Connection, table: &str, col: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for n in names {
        if n? == col {
            return Ok(true);
        }
    }
    Ok(false)
}

fn add_missing_columns(conn: &Connection, cols: &[(&str, &str, &str)]) -> rusqlite::Result<()> {
    for (table, col, decl) in cols {
        if !has_column(conn, table, col)? {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
        }
    }
    Ok(())
}

/// Make sure every v2 column and table exists, whichever shape the file has
/// (fresh v1, or v2 written by an earlier build). Idempotent.
fn ensure_v2_shape(conn: &Connection) -> rusqlite::Result<()> {
    add_missing_columns(conn, &SCHEMA_V2_COLUMNS)?;
    conn.execute_batch(SCHEMA_V2_TABLES)?;
    add_missing_columns(conn, &SCHEMA_V2_TABLE_COLUMNS)?;
    conn.execute_batch(SCHEMA_V2_INDEXES)
}

fn migrate_v2(conn: &Connection) -> rusqlite::Result<()> {
    ensure_v2_shape(conn)?;
    // C1 only hid deleted sessions; nothing cleaned up after them. Queue
    // their cleanup now (turns are normally gone already, so the vector and
    // fact steps are what matter).
    conn.execute_batch(
        "INSERT OR IGNORE INTO cascade(session_id, turn_ids, goal_ids, attempts, next_at, done)
         SELECT s.id,
                COALESCE((SELECT json_group_array(t.id) FROM turns t WHERE t.session_id = s.id), '[]'),
                COALESCE((SELECT json_group_array(DISTINCT t.goal_id) FROM turns t
                           WHERE t.session_id = s.id AND t.goal_id IS NOT NULL AND t.goal_id != ''), '[]'),
                0, 0, '[]'
         FROM sessions s WHERE s.state = 'deleting'",
    )
}

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
    let tx = conn.unchecked_transaction()?;
    if v < 2 {
        migrate_v2(&tx)?;
    } else {
        ensure_v2_shape(&tx)?;
    }
    ensure_v3_shape(&tx)?;
    tx.commit()?;
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
    fn open_creates_schema_v3() {
        let (_d, c) = open_temp();
        let v: i64 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);
        for t in [
            "sessions",
            "turns",
            "turns_fts",
            "tombstones",
            "meta",
            "cascade",
            "swept",
            "reviews",
        ] {
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

    /// A database written by C1 (schema v1, no semantic columns) opens, keeps
    /// every row and gains the v2 columns with their defaults.
    #[test]
    fn migrates_v1_to_v2_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chats.db");
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            c.execute_batch(
                "INSERT INTO sessions(id, kind, device, started_at, last_at, rev, turns, title, summary)
                   VALUES('s1','chat','lap',1,2,5,1,'T','S');
                 INSERT INTO turns(id, session_id, seq, at, user_text, answer_text, status, rev)
                   VALUES('t1','s1',1,2,'weather today','sunny','ok',5);
                 UPDATE meta SET value=5 WHERE key='rev';",
            )
            .unwrap();
            c.pragma_update(None, "user_version", 1).unwrap();
        }
        let c = open(&p).unwrap();
        let v: i64 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);
        let (title, sum_rev, manual, emb_rev): (String, i64, i64, i64) = c
            .query_row(
                "SELECT title, summarized_rev, title_manual, embedded_rev FROM sessions WHERE id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((title.as_str(), sum_rev, manual, emb_rev), ("T", 0, 0, 0));
        let embedded: i64 = c
            .query_row("SELECT embedded FROM turns WHERE id='t1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(embedded, 0);
        // FTS still works over the migrated row
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM turns_fts WHERE turns_fts MATCH 'weather'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        for t in ["cascade", "swept"] {
            assert!(c.prepare(&format!("SELECT * FROM {t}")).is_ok());
        }
        // reopening a v2 file is a no-op
        drop(c);
        assert!(open(&p).is_ok());
    }

    /// Sessions that C1 already deleted (state `deleting`) get their cleanup
    /// queued by the migration; live ones do not.
    #[test]
    fn migration_queues_cascade_for_deleting_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chats.db");
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            c.execute_batch(
                "INSERT INTO sessions(id, kind, device, started_at, last_at, rev, turns, state)
                   VALUES('dead','chat','lap',1,2,5,0,'deleting'),
                         ('dead2','chat','lap',1,2,6,1,'deleting'),
                         ('alive','chat','lap',1,2,7,1,'live');
                 INSERT INTO turns(id, session_id, seq, at, user_text, answer_text, status, goal_id, rev)
                   VALUES('t1','dead2',1,2,'a','b','ok','g9',6),
                         ('t2','alive',1,2,'a','b','ok','g1',7);",
            )
            .unwrap();
            c.pragma_update(None, "user_version", 1).unwrap();
        }
        let c = open(&p).unwrap();
        let rows: Vec<(String, String, String, i64)> = c
            .prepare(
                "SELECT session_id, turn_ids, goal_ids, next_at FROM cascade ORDER BY session_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("dead".into(), "[]".into(), "[]".into(), 0),
                ("dead2".into(), r#"["t1"]"#.into(), r#"["g9"]"#.into(), 0),
            ]
        );
        drop(c);
        let c = open(&p).unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM cascade", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "reopening does not requeue");
    }

    /// A v2 file written before `cascade.gen` and the `swept` retry columns
    /// existed gains them on open, keeping its rows.
    #[test]
    fn earlier_v2_shape_gains_gen_and_sweep_columns() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chats.db");
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            c.execute_batch(
                "ALTER TABLE sessions ADD COLUMN summarized_rev INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN title_manual INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN embedded_rev INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE turns ADD COLUMN embedded INTEGER NOT NULL DEFAULT 0;
                 CREATE TABLE cascade(session_id TEXT PRIMARY KEY, turn_ids TEXT NOT NULL DEFAULT '[]',
                   goal_ids TEXT NOT NULL DEFAULT '[]', attempts INTEGER NOT NULL DEFAULT 0,
                   next_at INTEGER NOT NULL DEFAULT 0, done TEXT NOT NULL DEFAULT '[]');
                 CREATE TABLE swept(session_id TEXT PRIMARY KEY, turn_ids TEXT NOT NULL DEFAULT '[]',
                   until INTEGER NOT NULL);
                 INSERT INTO cascade(session_id) VALUES('c1');
                 INSERT INTO swept(session_id, until) VALUES('w1', 99);",
            )
            .unwrap();
            c.pragma_update(None, "user_version", 2).unwrap();
        }
        let c = open(&p).unwrap();
        for (t, col) in [
            ("cascade", "gen"),
            ("swept", "swept_at"),
            ("swept", "failures"),
            ("swept", "next_at"),
            ("turns", "embed_attempts"),
        ] {
            assert!(has_column(&c, t, col).unwrap(), "{t}.{col}");
        }
        let (gen, until, swept_at): (i64, i64, i64) = c
            .query_row(
                "SELECT (SELECT gen FROM cascade WHERE session_id='c1'), until, swept_at FROM swept",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((gen, until, swept_at), (0, 99, 0));
        drop(c);
        assert!(open(&p).is_ok(), "reopening is a no-op");
    }

    /// A v2 database (C2) opens as v3: usage counters start at 0, cascade jobs
    /// keep working as `delete` jobs, the review table exists, data is intact.
    #[test]
    fn migrates_v2_to_v3_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chats.db");
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            migrate_v2(&c).unwrap();
            c.execute_batch(
                "INSERT INTO sessions(id, kind, device, started_at, last_at, rev, turns, title, summary, pinned)
                   VALUES('s1','chat','lap',1,2,5,1,'T','S',1);
                 INSERT INTO turns(id, session_id, seq, at, user_text, answer_text, status, rev)
                   VALUES('t1','s1',1,2,'weather today','sunny','ok',5);
                 INSERT INTO cascade(session_id, turn_ids) VALUES('gone','[\"x\"]');
                 UPDATE meta SET value=5 WHERE key='rev';",
            )
            .unwrap();
            c.pragma_update(None, "user_version", 2).unwrap();
        }
        let c = open(&p).unwrap();
        let v: i64 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);
        let (title, pinned, recall, used): (String, i64, i64, i64) = c
            .query_row(
                "SELECT title, pinned, recall_count, last_used_at FROM sessions WHERE id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((title.as_str(), pinned, recall, used), ("T", 1, 0, 0));
        let mode: String = c
            .query_row(
                "SELECT mode FROM cascade WHERE session_id='gone'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mode, "delete");
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM turns_fts WHERE turns_fts MATCH 'weather'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        assert!(c
            .prepare("SELECT id, created_at, status, groups FROM reviews")
            .is_ok());
        drop(c);
        assert!(open(&p).is_ok(), "reopening a v3 file is a no-op");
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
