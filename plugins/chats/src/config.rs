use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding `chats.db`.
    pub data_dir: PathBuf,
    /// Silence (seconds) after which a voice session is considered over.
    pub pause_secs: i64,
    /// Max characters kept for each tool_trace `args` / `result` / `error`.
    pub trace_max_chars: usize,
    /// Max bytes kept for `user_text` / `answer_text`.
    pub text_max_bytes: usize,
    /// Upper bound for a `chat_sync` page.
    pub sync_max: usize,
    /// `CHATS_SEMANTIC`: summaries, vectors, hybrid search and delete cascade.
    /// Off = C1 behaviour with no outbound kernel calls.
    pub semantic: bool,
    /// Worker tick (seconds).
    pub work_interval_secs: u64,
    /// A session is summarized once it has been silent this long (seconds).
    pub summary_idle_secs: i64,
    /// Optional `ai` agent profile used for summaries.
    pub ai_agent_id: Option<String>,
    pub summary_max_tokens: u32,
    /// Budget for the vector half of `chat_search` (ms).
    pub search_vec_timeout_ms: u32,
    /// Default `min_score` of `chat_recall`.
    pub recall_min_score: f64,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let data_dir = get("CHATS_DATA_DIR")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or("CHATS_DATA_DIR is required")?;
        fn num<T: std::str::FromStr + PartialOrd + Default>(
            get: &impl Fn(&str) -> Option<String>,
            key: &str,
            default: T,
        ) -> Result<T, String> {
            match get(key)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                None => Ok(default),
                Some(s) => match s.parse::<T>() {
                    Ok(v) if v > T::default() => Ok(v),
                    _ => Err(format!("{key} must be a positive integer, got {s:?}")),
                },
            }
        }
        let semantic = match get("CHATS_SEMANTIC")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .as_deref()
        {
            None | Some("on") => true,
            Some("off") => false,
            Some(o) => return Err(format!("CHATS_SEMANTIC must be on or off, got {o:?}")),
        };
        let ai_agent_id = get("CHATS_AI_AGENT_ID")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(Config {
            data_dir: PathBuf::from(data_dir),
            pause_secs: num(&get, "CHATS_PAUSE_SECS", 1800i64)?,
            trace_max_chars: num(&get, "CHATS_TRACE_MAX_CHARS", 200usize)?,
            text_max_bytes: num(&get, "CHATS_TEXT_MAX_BYTES", 65536usize)?,
            sync_max: num(&get, "CHATS_SYNC_MAX", 500usize)?,
            semantic,
            work_interval_secs: num(&get, "CHATS_WORK_INTERVAL_SECS", 60u64)?,
            summary_idle_secs: num(&get, "CHATS_SUMMARY_IDLE_SECS", 900i64)?,
            ai_agent_id,
            summary_max_tokens: num(&get, "CHATS_SUMMARY_MAX_TOKENS", 400u32)?,
            search_vec_timeout_ms: num(&get, "CHATS_SEARCH_VEC_TIMEOUT_MS", 800u32)?,
            recall_min_score: num(&get, "CHATS_RECALL_MIN_SCORE", 0.72f64)?,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("chats.db")
    }
}

#[cfg(test)]
pub(crate) fn test_config() -> Config {
    Config {
        data_dir: PathBuf::from("/nonexistent"),
        pause_secs: 1800,
        trace_max_chars: 200,
        text_max_bytes: 65536,
        sync_max: 500,
        semantic: true,
        work_interval_secs: 60,
        summary_idle_secs: 900,
        ai_agent_id: None,
        summary_max_tokens: 400,
        search_vec_timeout_ms: 800,
        recall_min_score: 0.72,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_data_dir() {
        assert!(Config::from_lookup(|_| None).is_err());
        assert!(Config::from_lookup(|k| (k == "CHATS_DATA_DIR").then(|| "  ".into())).is_err());
    }

    #[test]
    fn config_defaults_and_overrides() {
        let c = Config::from_lookup(|k| (k == "CHATS_DATA_DIR").then(|| "/x".into())).unwrap();
        assert_eq!(
            (
                c.pause_secs,
                c.trace_max_chars,
                c.text_max_bytes,
                c.sync_max
            ),
            (1800, 200, 65536, 500)
        );
        assert_eq!(c.db_path(), PathBuf::from("/x/chats.db"));
        let c = Config::from_lookup(|k| match k {
            "CHATS_DATA_DIR" => Some("/x".into()),
            "CHATS_PAUSE_SECS" => Some("60".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.pause_secs, 60);
        assert!(c.semantic);
        assert_eq!(
            (
                c.work_interval_secs,
                c.summary_idle_secs,
                c.summary_max_tokens
            ),
            (60, 900, 400)
        );
        assert_eq!((c.search_vec_timeout_ms, c.recall_min_score), (800, 0.72));
        assert_eq!(c.ai_agent_id, None);
        let c = Config::from_lookup(|k| match k {
            "CHATS_DATA_DIR" => Some("/x".into()),
            "CHATS_SEMANTIC" => Some("OFF".into()),
            "CHATS_AI_AGENT_ID" => Some(" summarizer ".into()),
            "CHATS_RECALL_MIN_SCORE" => Some("0.5".into()),
            _ => None,
        })
        .unwrap();
        assert!(!c.semantic);
        assert_eq!(c.ai_agent_id.as_deref(), Some("summarizer"));
        assert_eq!(c.recall_min_score, 0.5);
        assert!(Config::from_lookup(|k| match k {
            "CHATS_DATA_DIR" => Some("/x".into()),
            "CHATS_SEMANTIC" => Some("maybe".into()),
            _ => None,
        })
        .is_err());
        assert!(Config::from_lookup(|k| match k {
            "CHATS_DATA_DIR" => Some("/x".into()),
            "CHATS_SYNC_MAX" => Some("0".into()),
            _ => None,
        })
        .is_err());
    }
}
