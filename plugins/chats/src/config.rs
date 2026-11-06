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
            match get(key).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
                None => Ok(default),
                Some(s) => match s.parse::<T>() {
                    Ok(v) if v > T::default() => Ok(v),
                    _ => Err(format!("{key} must be a positive integer, got {s:?}")),
                },
            }
        }
        Ok(Config {
            data_dir: PathBuf::from(data_dir),
            pause_secs: num(&get, "CHATS_PAUSE_SECS", 1800i64)?,
            trace_max_chars: num(&get, "CHATS_TRACE_MAX_CHARS", 200usize)?,
            text_max_bytes: num(&get, "CHATS_TEXT_MAX_BYTES", 65536usize)?,
            sync_max: num(&get, "CHATS_SYNC_MAX", 500usize)?,
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
            (c.pause_secs, c.trace_max_chars, c.text_max_bytes, c.sync_max),
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
        assert!(Config::from_lookup(|k| match k {
            "CHATS_DATA_DIR" => Some("/x".into()),
            "CHATS_SYNC_MAX" => Some("0".into()),
            _ => None,
        })
        .is_err());
    }
}
