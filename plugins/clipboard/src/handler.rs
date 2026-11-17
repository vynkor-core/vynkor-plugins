//! Action handlers for the `clipboard` plugin: `clipboard_read`,
//! `clipboard_write`, `clipboard_providers`. All flows go through
//! [`providers::Runner`] so tests run without a real clipboard.

use serde_json::{json, Value};

use crate::providers::{parse_provider_pref, read_chain, write_chain, Runner, Session};

/// Operator policy resolved once per call from env (constructed directly in
/// tests).
#[derive(Debug, Clone)]
pub struct Config {
    pub timeout_ms: u64,
    pub max_bytes: usize,
    pub provider_pref: crate::providers::ProviderPref,
}

impl Config {
    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok();
        Self {
            timeout_ms: env(crate::providers::TIMEOUT_MS_ENV)
                .and_then(|v| v.trim().parse().ok())
                .filter(|&ms| ms > 0)
                .unwrap_or(crate::providers::DEFAULT_TIMEOUT_MS),
            max_bytes: env(crate::providers::MAX_BYTES_ENV)
                .and_then(|v| v.trim().parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(crate::providers::DEFAULT_MAX_BYTES),
            provider_pref: parse_provider_pref(env(crate::providers::PROVIDER_ENV).as_deref())
                .unwrap_or(crate::providers::ProviderPref::Auto),
        }
    }
}

pub async fn handle_read(
    runner: &dyn Runner,
    cfg: &Config,
    session: Session,
) -> Result<Value, String> {
    if cfg.provider_pref == crate::providers::ProviderPref::X11 {
        return read_session(runner, cfg, Session::X11).await;
    }
    if cfg.provider_pref == crate::providers::ProviderPref::Wayland {
        return read_session(runner, cfg, Session::Wayland).await;
    }
    read_session(runner, cfg, session).await
}

async fn read_session(
    runner: &dyn Runner,
    cfg: &Config,
    session: Session,
) -> Result<Value, String> {
    let chain = read_chain(session);
    let mut tried: Vec<&'static str> = Vec::new();
    for inv in &chain {
        match runner.run(inv.bin, &inv.args, None, cfg.timeout_ms).await {
            Ok(bytes) => {
                if bytes.len() > cfg.max_bytes {
                    return Err(format!(
                        "ERR_CLIPBOARD_TOO_LARGE: {} bytes exceeds CLIPBOARD_PLUGIN_MAX_BYTES={}",
                        bytes.len(),
                        cfg.max_bytes
                    ));
                }
                let text = String::from_utf8(bytes).map_err(|_| {
                    "ERR_CLIPBOARD_READ_FAILED: clipboard content is not valid UTF-8".to_string()
                })?;
                if text.is_empty() {
                    return Ok(json!({ "found": false, "text": Value::Null, "provider": inv.bin }));
                }
                return Ok(json!({ "found": true, "text": text, "provider": inv.bin }));
            }
            Err(e) if e.contains("ERR_CLIPBOARD_PROVIDER_MISSING") => {
                tried.push(inv.bin);
            }
            Err(e) => return Err(e),
        }
    }
    Err(format!(
        "ERR_CLIPBOARD_PROVIDER_MISSING: no working reader for {} (tried: {})",
        session.as_str(),
        tried.join(", ")
    ))
}

pub async fn handle_write(
    runner: &dyn Runner,
    cfg: &Config,
    session: Session,
    text: &str,
) -> Result<Value, String> {
    if text.is_empty() {
        return Err(
            "ERR_CLIPBOARD_BAD_PARAMS: 'text' must be a non-empty string (empty writes are rejected to avoid accidental clears)".to_string(),
        );
    }
    if text.len() > cfg.max_bytes {
        return Err(format!(
            "ERR_CLIPBOARD_TOO_LARGE: {} bytes exceeds CLIPBOARD_PLUGIN_MAX_BYTES={}",
            text.len(),
            cfg.max_bytes
        ));
    }

    let chain = write_chain(match cfg.provider_pref {
        crate::providers::ProviderPref::X11 => Session::X11,
        crate::providers::ProviderPref::Wayland => Session::Wayland,
        crate::providers::ProviderPref::Auto => session,
    });
    let mut tried: Vec<&'static str> = Vec::new();
    for inv in &chain {
        let payload = if inv.stdin { Some(text) } else { None };
        match runner
            .run(inv.bin, &inv.args, payload, cfg.timeout_ms)
            .await
        {
            Ok(_) => return Ok(json!({ "ok": true, "provider": inv.bin, "bytes": text.len() })),
            Err(e) if e.contains("ERR_CLIPBOARD_PROVIDER_MISSING") => {
                tried.push(inv.bin);
            }
            Err(e) => return Err(e),
        }
    }
    Err(format!(
        "ERR_CLIPBOARD_PROVIDER_MISSING: no working writer for {} (tried: {})",
        session.as_str(),
        tried.join(", ")
    ))
}

pub fn handle_providers(session: Session) -> Value {
    json!({
        "session": session.as_str(),
        "readers": read_chain(session).iter().map(|i| i.bin).collect::<Vec<_>>(),
        "writers": write_chain(session).iter().map(|i| i.bin).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{FakeRunner, ProviderPref};
    use std::collections::HashMap;

    fn cfg(pref: ProviderPref, max_bytes: usize) -> Config {
        Config {
            timeout_ms: 1_000,
            max_bytes,
            provider_pref: pref,
        }
    }

    fn fake(pairs: Vec<(&str, Result<Vec<u8>, String>)>) -> FakeRunner {
        let mut results = HashMap::new();
        for (bin, r) in pairs {
            results.insert(bin.to_string(), r);
        }
        FakeRunner::new(results)
    }

    #[tokio::test]
    async fn read_success_wayland() {
        let f = fake(vec![("wl-paste", Ok(b"hello".to_vec()))]);
        let v = handle_read(&f, &cfg(ProviderPref::Auto, 1024), Session::Wayland)
            .await
            .unwrap();
        assert_eq!(v["found"], true);
        assert_eq!(v["text"], "hello");
        assert_eq!(v["provider"], "wl-paste");
    }

    #[tokio::test]
    async fn read_empty_clipboard_reports_found_false() {
        let f = fake(vec![("wl-paste", Ok(Vec::new()))]);
        let v = handle_read(&f, &cfg(ProviderPref::Auto, 1024), Session::Wayland)
            .await
            .unwrap();
        assert_eq!(v["found"], false);
        assert_eq!(v["text"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn read_oversized_rejected() {
        let f = fake(vec![("wl-paste", Ok(vec![b'a'; 4096]))]);
        let err = handle_read(&f, &cfg(ProviderPref::Auto, 1024), Session::Wayland)
            .await
            .unwrap_err();
        assert!(err.contains("ERR_CLIPBOARD_TOO_LARGE"), "{err}");
    }

    #[tokio::test]
    async fn read_falls_back_from_xclip_to_xsel() {
        let f = fake(vec![
            (
                "xclip",
                Err("ERR_CLIPBOARD_PROVIDER_MISSING: binary 'xclip' not found on PATH".into()),
            ),
            ("xsel", Ok(b"fallback".to_vec())),
        ]);
        let v = handle_read(&f, &cfg(ProviderPref::Auto, 1024), Session::X11)
            .await
            .unwrap();
        assert_eq!(v["text"], "fallback");
        assert_eq!(v["provider"], "xsel");
    }

    #[tokio::test]
    async fn read_all_readers_missing_lists_tried() {
        let missing = |b: &str| {
            Err(format!(
                "ERR_CLIPBOARD_PROVIDER_MISSING: binary '{b}' not found on PATH"
            ))
        };
        let f = fake(vec![("xclip", missing("xclip")), ("xsel", missing("xsel"))]);
        let err = handle_read(&f, &cfg(ProviderPref::Auto, 1024), Session::X11)
            .await
            .unwrap_err();
        assert!(err.contains("ERR_CLIPBOARD_PROVIDER_MISSING"), "{err}");
        assert!(err.contains("xclip") && err.contains("xsel"), "{err}");
    }

    #[tokio::test]
    async fn read_non_utf8_rejected() {
        let f = fake(vec![("wl-paste", Ok(vec![0xff, 0xfe]))]);
        let err = handle_read(&f, &cfg(ProviderPref::Auto, 1024), Session::Wayland)
            .await
            .unwrap_err();
        assert!(err.contains("ERR_CLIPBOARD_READ_FAILED"), "{err}");
    }

    #[tokio::test]
    async fn read_error_other_than_missing_propagates() {
        let f = fake(vec![(
            "wl-paste",
            Err("ERR_CLIPBOARD_TIMEOUT: boom".into()),
        )]);
        let err = handle_read(&f, &cfg(ProviderPref::Auto, 1024), Session::Wayland)
            .await
            .unwrap_err();
        assert!(err.contains("ERR_CLIPBOARD_TIMEOUT"), "{err}");
    }

    #[tokio::test]
    async fn write_success_passes_text_via_stdin() {
        let f = fake(vec![("wl-copy", Ok(Vec::new()))]);
        let v = handle_write(
            &f,
            &cfg(ProviderPref::Auto, 1024),
            Session::Wayland,
            "payload",
        )
        .await
        .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["provider"], "wl-copy");
        let calls = f.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2.as_deref(), Some("payload"));
    }

    #[tokio::test]
    async fn write_empty_rejected() {
        let f = fake(vec![("wl-copy", Ok(Vec::new()))]);
        let err = handle_write(&f, &cfg(ProviderPref::Auto, 1024), Session::Wayland, "")
            .await
            .unwrap_err();
        assert!(err.contains("ERR_CLIPBOARD_BAD_PARAMS"), "{err}");
    }

    #[tokio::test]
    async fn write_oversized_rejected_before_spawn() {
        let f = fake(vec![("wl-copy", Ok(Vec::new()))]);
        let big = "x".repeat(2048);
        let err = handle_write(&f, &cfg(ProviderPref::Auto, 1024), Session::Wayland, &big)
            .await
            .unwrap_err();
        assert!(err.contains("ERR_CLIPBOARD_TOO_LARGE"), "{err}");
        assert!(f.calls.lock().unwrap().is_empty(), "must not spawn");
    }

    #[tokio::test]
    async fn write_all_writers_missing_lists_tried() {
        let missing = |b: &str| {
            Err(format!(
                "ERR_CLIPBOARD_PROVIDER_MISSING: binary '{b}' not found on PATH"
            ))
        };
        let f = fake(vec![("xclip", missing("xclip")), ("xsel", missing("xsel"))]);
        let err = handle_write(&f, &cfg(ProviderPref::Auto, 1024), Session::X11, "t")
            .await
            .unwrap_err();
        assert!(err.contains("xclip") && err.contains("xsel"), "{err}");
    }

    #[tokio::test]
    async fn provider_pref_overrides_detected_session() {
        let f = fake(vec![("wl-paste", Ok(b"wayland-text".to_vec()))]);
        let v = handle_read(&f, &cfg(ProviderPref::Wayland, 1024), Session::X11)
            .await
            .unwrap();
        assert_eq!(v["provider"], "wl-paste");
    }
}
