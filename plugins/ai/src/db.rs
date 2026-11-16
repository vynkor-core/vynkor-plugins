//! SQLite persistence for the `ai` plugin: declared/discovered models, agent
//! profiles, and per-call token usage for analytics. One file (`ai.db`) under
//! the kernel-granted plugin data dir (`VYN_DATA_DIR`); falls back to an
//! in-memory database when the kernel doesn't provide one (usage then does
//! not survive a restart).

use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// A model the `ai` plugin can complete with. `base_url`/`api_key_env` are
/// host-side: the phone never sees them — the plugin resolves them here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub discovered_at: Option<i64>,
    #[serde(default)]
    pub last_seen: i64,
}

/// A named agent profile: a model plus the behavior framing around it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub model_id: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct UsageRow {
    pub agent_id: String,
    pub model_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Kernel-stamped plugin id of the requester (`caller_plugin_id`) —
    /// unspoofable, so embeddings from `vector-db` are told apart from
    /// `agent`'s chats even when neither names an `agent_id`.
    pub caller: String,
    /// Caller-supplied label for why the call was made (`agent` sends
    /// `goal_step` | `memory_extract` | `small_talk`); free-form, may be empty.
    pub purpose: String,
    /// Caller-supplied correlation id (`agent`'s goal id); may be empty.
    pub goal_id: String,
    /// Wall time of the provider round-trip, measured here.
    pub latency_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct UsageBucket {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct UsageStats {
    pub totals: UsageBucket,
    pub by_model: Vec<(String, UsageBucket)>,
    pub by_agent: Vec<(String, UsageBucket)>,
    #[serde(default)]
    pub by_caller: Vec<(String, UsageBucket)>,
    #[serde(default)]
    pub by_purpose: Vec<(String, UsageBucket)>,
}

/// Columns added to `usage` after the first release. `CREATE TABLE IF NOT
/// EXISTS` never alters an existing table, so `open` adds whichever of
/// these an older `ai.db` lacks (rows recorded before then keep the default).
const USAGE_ADDED_COLUMNS: &[(&str, &str)] = &[
    ("caller", "TEXT NOT NULL DEFAULT ''"),
    ("purpose", "TEXT NOT NULL DEFAULT ''"),
    ("goal_id", "TEXT NOT NULL DEFAULT ''"),
    ("latency_ms", "INTEGER NOT NULL DEFAULT 0"),
];

fn migrate_usage(conn: &Connection) -> rusqlite::Result<()> {
    let existing: Vec<String> = conn
        .prepare("PRAGMA table_info(usage)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    for (name, decl) in USAGE_ADDED_COLUMNS {
        if !existing.iter().any(|c| c == name) {
            conn.execute_batch(&format!("ALTER TABLE usage ADD COLUMN {name} {decl}"))?;
        }
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_usage_goal ON usage(goal_id);")
}

/// `SELECT <column>, COUNT, SUM(in), SUM(out) ... GROUP BY <column>`.
/// `column` is always one of this file's literals, never caller input.
fn usage_grouped_by(conn: &Connection, column: &str) -> rusqlite::Result<Vec<(String, UsageBucket)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {column}, COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0)
         FROM usage GROUP BY {column} ORDER BY 2 DESC"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            UsageBucket {
                requests: r.get::<_, i64>(1)? as u64,
                input_tokens: r.get::<_, i64>(2)? as u64,
                output_tokens: r.get::<_, i64>(3)? as u64,
            },
        ))
    })?;
    rows.collect()
}

pub struct AiDb {
    conn: Mutex<Connection>,
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl AiDb {
    /// Open (or create) `ai.db` under `data_dir`. With `None`, use an
    /// in-memory database — fine for running without a kernel data dir, at
    /// the cost of non-persistent usage/analytics.
    pub fn open(data_dir: Option<&Path>) -> anyhow::Result<Self> {
        let conn = match data_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                Connection::open(dir.join("ai.db"))?
            }
            None => Connection::open_in_memory()?,
        };
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS models (
                id            TEXT PRIMARY KEY,
                provider      TEXT NOT NULL,
                base_url      TEXT NOT NULL,
                api_key_env   TEXT NOT NULL,
                is_default    INTEGER NOT NULL DEFAULT 0,
                discovered_at INTEGER,
                last_seen     INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS agents (
                id            TEXT PRIMARY KEY,
                name          TEXT NOT NULL,
                model_id      TEXT NOT NULL,
                system_prompt TEXT NOT NULL DEFAULT '',
                goal          TEXT NOT NULL DEFAULT '',
                description   TEXT NOT NULL DEFAULT '',
                is_default    INTEGER NOT NULL DEFAULT 0,
                created_at    INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS usage (
                id            TEXT PRIMARY KEY,
                agent_id      TEXT NOT NULL DEFAULT '',
                model_id      TEXT NOT NULL,
                input_tokens  INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                created_at    INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_usage_model ON usage(model_id);
            CREATE INDEX IF NOT EXISTS idx_usage_agent ON usage(agent_id);
            CREATE INDEX IF NOT EXISTS idx_usage_created ON usage(created_at);",
        )?;
        migrate_usage(&conn)?;
        Ok(AiDb {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ------------------------------------------------------------- models

    pub fn upsert_model(&self, m: &Model) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO models (id, provider, base_url, api_key_env, is_default, discovered_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                provider = excluded.provider,
                base_url = excluded.base_url,
                api_key_env = excluded.api_key_env,
                is_default = excluded.is_default,
                discovered_at = excluded.discovered_at,
                last_seen = excluded.last_seen",
            params![
                m.id,
                m.provider,
                m.base_url,
                m.api_key_env,
                m.is_default as i64,
                m.discovered_at,
                m.last_seen,
            ],
        )?;
        Ok(())
    }

    pub fn get_model(&self, id: &str) -> anyhow::Result<Option<Model>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, provider, base_url, api_key_env, is_default, discovered_at, last_seen
             FROM models WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_model)?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_models(&self) -> anyhow::Result<Vec<Model>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, provider, base_url, api_key_env, is_default, discovered_at, last_seen
             FROM models ORDER BY is_default DESC, id",
        )?;
        let rows = stmt.query_map([], row_to_model)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn default_model(&self) -> anyhow::Result<Option<Model>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, provider, base_url, api_key_env, is_default, discovered_at, last_seen
             FROM models WHERE is_default = 1 LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], row_to_model)?;
        Ok(rows.next().transpose()?)
    }

    pub fn touch_model(&self, id: &str) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE models SET last_seen = ?1 WHERE id = ?2",
            params![now_millis(), id],
        )?;
        Ok(())
    }

    pub fn delete_model(&self, id: &str) -> anyhow::Result<()> {
        self.conn()
            .execute("DELETE FROM models WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Make `id` the single default model (clears the flag on all others).
    pub fn set_model_default(&self, id: &str) -> anyhow::Result<()> {
        self.conn()
            .execute("UPDATE models SET is_default = 0", [])?;
        self.conn().execute(
            "UPDATE models SET is_default = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------- agents

    pub fn upsert_agent(&self, a: &Agent) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO agents (id, name, model_id, system_prompt, goal, description, is_default, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                model_id = excluded.model_id,
                system_prompt = excluded.system_prompt,
                goal = excluded.goal,
                description = excluded.description,
                is_default = excluded.is_default,
                created_at = excluded.created_at",
            params![
                a.id,
                a.name,
                a.model_id,
                a.system_prompt,
                a.goal,
                a.description,
                a.is_default as i64,
                a.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_agent(&self, id: &str) -> anyhow::Result<Option<Agent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, model_id, system_prompt, goal, description, is_default, created_at
             FROM agents WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_agent)?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_agents(&self) -> anyhow::Result<Vec<Agent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, model_id, system_prompt, goal, description, is_default, created_at
             FROM agents ORDER BY is_default DESC, id",
        )?;
        let rows = stmt.query_map([], row_to_agent)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn default_agent(&self) -> anyhow::Result<Option<Agent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, model_id, system_prompt, goal, description, is_default, created_at
             FROM agents WHERE is_default = 1 LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], row_to_agent)?;
        Ok(rows.next().transpose()?)
    }

    pub fn delete_agent(&self, id: &str) -> anyhow::Result<()> {
        self.conn()
            .execute("DELETE FROM agents WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Make `id` the single default agent (clears the flag on all others).
    pub fn set_agent_default(&self, id: &str) -> anyhow::Result<()> {
        self.conn()
            .execute("UPDATE agents SET is_default = 0", [])?;
        self.conn().execute(
            "UPDATE agents SET is_default = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------- usage

    pub fn record_usage(&self, row: &UsageRow) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO usage (id, agent_id, model_id, input_tokens, output_tokens, created_at,
                                caller, purpose, goal_id, latency_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                uuid::Uuid::new_v4().to_string(),
                row.agent_id,
                row.model_id,
                row.input_tokens as i64,
                row.output_tokens as i64,
                now_millis(),
                row.caller,
                row.purpose,
                row.goal_id,
                row.latency_ms as i64,
            ],
        )?;
        Ok(())
    }

    pub fn usage_stats(&self) -> anyhow::Result<UsageStats> {
        let mut stats = UsageStats::default();
        {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0)
                 FROM usage",
            )?;
            if let Ok(row) = stmt.query_row([], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                ))
            }) {
                stats.totals = UsageBucket {
                    requests: row.0,
                    input_tokens: row.1,
                    output_tokens: row.2,
                };
            }

            stats.by_model = usage_grouped_by(&conn, "model_id")?;
            stats.by_agent = usage_grouped_by(&conn, "agent_id")?;
            stats.by_caller = usage_grouped_by(&conn, "caller")?;
            stats.by_purpose = usage_grouped_by(&conn, "purpose")?;
        }
        Ok(stats)
    }
}

fn row_to_model(row: &rusqlite::Row<'_>) -> rusqlite::Result<Model> {
    Ok(Model {
        id: row.get(0)?,
        provider: row.get(1)?,
        base_url: row.get(2)?,
        api_key_env: row.get(3)?,
        is_default: row.get::<_, i64>(4)? != 0,
        discovered_at: row.get(5)?,
        last_seen: row.get(6)?,
    })
}

fn row_to_agent(row: &rusqlite::Row<'_>) -> rusqlite::Result<Agent> {
    Ok(Agent {
        id: row.get(0)?,
        name: row.get(1)?,
        model_id: row.get(2)?,
        system_prompt: row.get(3)?,
        goal: row.get(4)?,
        description: row.get(5)?,
        is_default: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> AiDb {
        AiDb::open(None).unwrap()
    }

    fn model(id: &str) -> Model {
        Model {
            id: id.to_string(),
            provider: "openai".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            is_default: false,
            discovered_at: None,
            last_seen: 0,
        }
    }

    fn agent(id: &str, model_id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            name: id.to_string(),
            model_id: model_id.to_string(),
            system_prompt: "be helpful".to_string(),
            goal: String::new(),
            description: String::new(),
            is_default: false,
            created_at: now_millis(),
        }
    }

    #[test]
    fn upsert_model_then_list() {
        let d = db();
        let m = model("llama3.2");
        d.upsert_model(&m).unwrap();
        let list = d.list_models().unwrap();
        assert_eq!(list, vec![m]);
    }

    #[test]
    fn upsert_model_is_idempotent() {
        let d = db();
        d.upsert_model(&model("a")).unwrap();
        d.upsert_model(&model("a")).unwrap();
        assert_eq!(d.list_models().unwrap().len(), 1);
    }

    #[test]
    fn default_model_returns_none_when_unset() {
        let d = db();
        assert!(d.default_model().unwrap().is_none());
    }

    #[test]
    fn get_model_missing_is_none() {
        let d = db();
        assert!(d.get_model("nope").unwrap().is_none());
    }

    #[test]
    fn delete_model_removes() {
        let d = db();
        d.upsert_model(&model("x")).unwrap();
        d.delete_model("x").unwrap();
        assert!(d.get_model("x").unwrap().is_none());
    }

    #[test]
    fn agent_crud() {
        let d = db();
        let a = agent("code", "qwen2.5-coder");
        d.upsert_agent(&a).unwrap();
        assert_eq!(d.get_agent("code").unwrap(), Some(a.clone()));
        assert_eq!(d.list_agents().unwrap(), vec![a]);
        d.delete_agent("code").unwrap();
        assert!(d.get_agent("code").unwrap().is_none());
    }

    #[test]
    fn usage_stats_aggregates_by_model_and_agent() {
        let d = db();
        d.record_usage(&UsageRow {
            agent_id: "code".into(),
            model_id: "qwen".into(),
            input_tokens: 10,
            output_tokens: 5,
            purpose: "goal_step".into(),
            ..Default::default()
        })
        .unwrap();
        d.record_usage(&UsageRow {
            agent_id: "code".into(),
            model_id: "qwen".into(),
            input_tokens: 2,
            output_tokens: 3,
            purpose: "memory_extract".into(),
            ..Default::default()
        })
        .unwrap();
        d.record_usage(&UsageRow {
            agent_id: String::new(),
            model_id: "llama".into(),
            input_tokens: 7,
            output_tokens: 1,
            caller: "vector-db".into(),
            ..Default::default()
        })
        .unwrap();

        let s = d.usage_stats().unwrap();
        assert_eq!(s.totals.requests, 3);
        assert_eq!(s.totals.input_tokens, 19);
        assert_eq!(s.totals.output_tokens, 9);

        let qwen = s
            .by_model
            .iter()
            .find(|(id, _)| id == "qwen")
            .unwrap()
            .1
            .clone();
        assert_eq!(qwen.requests, 2);
        assert_eq!(qwen.input_tokens, 12);

        let code = s
            .by_agent
            .iter()
            .find(|(id, _)| id == "code")
            .unwrap()
            .1
            .clone();
        assert_eq!(code.requests, 2);
        assert_eq!(code.output_tokens, 8);

        let bucket = |v: &[(String, UsageBucket)], k: &str| {
            v.iter().find(|(id, _)| id == k).map(|(_, b)| b.clone()).unwrap()
        };
        assert_eq!(bucket(&s.by_purpose, "memory_extract").input_tokens, 2);
        assert_eq!(bucket(&s.by_purpose, "").requests, 1);
        assert_eq!(bucket(&s.by_caller, "vector-db").input_tokens, 7);
    }

    #[test]
    fn migrate_adds_attribution_columns_to_a_pre_existing_usage_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage (
                id TEXT PRIMARY KEY, agent_id TEXT NOT NULL DEFAULT '',
                model_id TEXT NOT NULL, input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL, created_at INTEGER NOT NULL
            );
            INSERT INTO usage VALUES ('old', 'default', 'm', 100, 1, 0);",
        )
        .unwrap();
        migrate_usage(&conn).unwrap();
        // idempotent: a second open must not try to re-add the columns
        migrate_usage(&conn).unwrap();
        let (purpose, latency): (String, i64) = conn
            .query_row("SELECT purpose, latency_ms FROM usage WHERE id='old'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((purpose.as_str(), latency), ("", 0));
    }
}
