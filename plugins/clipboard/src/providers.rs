//! Clipboard backends: `wl-paste`/`wl-copy` (Wayland) and
//! `xclip`/`xsel` (X11). Every access spawns the binary directly with argv —
//! never a shell — so clipboard content cannot inject commands. The session
//! type is detected once from the environment; operator policy lives in the
//! `CLIPBOARD_PLUGIN_PROVIDER` env var (`auto`/`wayland`/`x11`).

#[cfg(test)]
use std::collections::HashMap;
use std::process::{ExitStatus, Stdio};
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Operator env var: provider preference, `auto` (default), `wayland` or `x11`.
pub const PROVIDER_ENV: &str = "CLIPBOARD_PLUGIN_PROVIDER";
/// Operator env var: per-spawn timeout in milliseconds.
pub const TIMEOUT_MS_ENV: &str = "CLIPBOARD_PLUGIN_TIMEOUT_MS";
/// Operator env var: hard cap on clipboard payload size in bytes.
pub const MAX_BYTES_ENV: &str = "CLIPBOARD_PLUGIN_MAX_BYTES";

pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_MAX_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Wayland,
    X11,
}

impl Session {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wayland => "wayland",
            Self::X11 => "x11",
        }
    }
}

/// Detect the graphical session from environment values passed in by the
/// caller (kept pure so tests never touch process env).
///
/// Order: `WAYLAND_DISPLAY` wins, then an explicit
/// `XDG_SESSION_TYPE=x11`, then any `DISPLAY`. Nothing set → error.
pub fn detect_session(
    wayland_display: Option<&str>,
    session_type: Option<&str>,
    display: Option<&str>,
) -> Result<Session, String> {
    if wayland_display
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        return Ok(Session::Wayland);
    }
    if matches!(session_type.map(str::trim), Some("x11")) {
        return Ok(Session::X11);
    }
    if display.map(|s| !s.trim().is_empty()).unwrap_or(false) {
        return Ok(Session::X11);
    }
    Err(
        "ERR_CLIPBOARD_NO_SESSION: no graphical session detected (set WAYLAND_DISPLAY or DISPLAY)"
            .to_string(),
    )
}

pub fn detect_session_from_env() -> Result<Session, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
    detect_session(
        env("WAYLAND_DISPLAY").as_deref(),
        env("XDG_SESSION_TYPE").as_deref(),
        env("DISPLAY").as_deref(),
    )
}

/// Operator provider preference parsed from `CLIPBOARD_PLUGIN_PROVIDER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderPref {
    Auto,
    Wayland,
    X11,
}

pub fn parse_provider_pref(s: Option<&str>) -> Result<ProviderPref, String> {
    match s.map(str::trim).unwrap_or("") {
        "" | "auto" => Ok(ProviderPref::Auto),
        "wayland" => Ok(ProviderPref::Wayland),
        "x11" => Ok(ProviderPref::X11),
        other => Err(format!(
            "ERR_CLIPBOARD_BAD_PARAMS: invalid {PROVIDER_ENV} '{other}' (expected auto/wayland/x11)"
        )),
    }
}

/// One concrete backend binary invocation for a read or write.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub bin: &'static str,
    pub args: Vec<&'static str>,
    pub stdin: bool,
}

pub fn read_chain(session: Session) -> Vec<Invocation> {
    match session {
        Session::Wayland => vec![Invocation {
            bin: "wl-paste",
            args: vec!["--no-newline"],
            stdin: false,
        }],
        Session::X11 => vec![
            Invocation {
                bin: "xclip",
                args: vec!["-selection", "clipboard", "-out"],
                stdin: false,
            },
            Invocation {
                bin: "xsel",
                args: vec!["--clipboard", "--output"],
                stdin: false,
            },
        ],
    }
}

pub fn write_chain(session: Session) -> Vec<Invocation> {
    match session {
        Session::Wayland => vec![Invocation {
            bin: "wl-copy",
            args: vec![],
            stdin: true,
        }],
        Session::X11 => vec![
            Invocation {
                bin: "xclip",
                args: vec!["-selection", "clipboard", "-in"],
                stdin: true,
            },
            Invocation {
                bin: "xsel",
                args: vec!["--clipboard", "--input"],
                stdin: true,
            },
        ],
    }
}

/// Process execution boundary — mocked in tests so CI never touches a real
/// compositor clipboard.
#[async_trait]
pub trait Runner: Send + Sync {
    /// Spawn `bin args`, feed `stdin` when present, wait up to
    /// `timeout_ms`, return stdout. Errors carry the `ERR_CLIPBOARD_*`
    /// taxonomy.
    async fn run(
        &self,
        bin: &str,
        args: &[&str],
        stdin: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, String>;
}

pub struct RealRunner;

/// After a writer's direct child exits, give its pipes this long to reach
/// EOF before proceeding with whatever was flushed. Tools that fork a daemon
/// (wl-copy) never reach EOF — the inherited pipe copies die with the daemon
/// — so this bounds the wait instead of hanging forever.
const PIPE_GRACE_MS: u64 = 150;

#[async_trait]
impl Runner for RealRunner {
    async fn run(
        &self,
        bin: &str,
        args: &[&str],
        stdin: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, String> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("ERR_CLIPBOARD_PROVIDER_MISSING: binary '{bin}' not found on PATH")
            } else {
                format!("ERR_CLIPBOARD_WRITE_FAILED: spawn '{bin}' failed: {e}")
            }
        })?;

        let outcome =
            tokio::time::timeout(Duration::from_millis(timeout_ms), async {
                if let Some(text) = stdin {
                    if let Some(mut handle) = child.stdin.take() {
                        handle.write_all(text.as_bytes()).await.map_err(|e| {
                            format!("ERR_CLIPBOARD_WRITE_FAILED: stdin to '{bin}' failed: {e}")
                        })?;
                        handle.shutdown().await.ok();
                    }
                }

                let mut out_buf = Vec::new();
                let mut err_buf = Vec::new();
                let mut stdout_pipe = child.stdout.take();
                let mut stderr_pipe = child.stderr.take();

                let drain_stdout = async {
                    if let Some(p) = stdout_pipe.as_mut() {
                        let _ = tokio::io::AsyncReadExt::read_to_end(p, &mut out_buf).await;
                    }
                };
                let drain_stderr = async {
                    if let Some(p) = stderr_pipe.as_mut() {
                        let _ = tokio::io::AsyncReadExt::read_to_end(p, &mut err_buf).await;
                    }
                };

                if stdin.is_some() {
                    let status = child.wait().await.map_err(|e| {
                        format!("ERR_CLIPBOARD_READ_FAILED: wait '{bin}' failed: {e}")
                    })?;
                    if !status.success() {
                        // A failed writer exits without daemonizing, so its
                        // pipes hit EOF on their own; the bound only guards a
                        // misbehaving provider.
                        let _ = tokio::time::timeout(Duration::from_millis(PIPE_GRACE_MS), async {
                            tokio::join!(drain_stdout, drain_stderr)
                        })
                        .await;
                    }
                    Ok((out_buf, err_buf, status))
                } else {
                    let (_, _, res) = tokio::join!(drain_stdout, drain_stderr, async {
                        child.wait().await.map_err(|e| {
                            format!("ERR_CLIPBOARD_READ_FAILED: wait '{bin}' failed: {e}")
                        })
                    });
                    let status = res?;
                    Ok((out_buf, err_buf, status))
                }
            })
            .await;

        match outcome {
            Err(_) => Err(format!(
                "ERR_CLIPBOARD_TIMEOUT: '{bin}' exceeded {timeout_ms}ms"
            )),
            Ok(Err(msg)) => Err(msg),
            Ok(Ok((_, err_buf, status))) if !status.success() => {
                let stderr = String::from_utf8_lossy(&err_buf);
                let trimmed = stderr.trim();
                Err(format!(
                    "ERR_CLIPBOARD_READ_FAILED: '{bin}' exited with {status}: {}",
                    if trimmed.is_empty() {
                        "(no stderr)"
                    } else {
                        trimmed
                    }
                ))
            }
            Ok(Ok((out_buf, _, _))) => Ok(out_buf),
        }
    }
}

/// Test double: canned results keyed by binary name, records every call.
#[cfg(test)]
pub struct FakeRunner {
    pub results: HashMap<String, Result<Vec<u8>, String>>,
    pub calls: Mutex<Vec<(String, Vec<String>, Option<String>)>>,
}

#[cfg(test)]
impl FakeRunner {
    pub fn new(results: HashMap<String, Result<Vec<u8>, String>>) -> Self {
        Self {
            results,
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl Runner for FakeRunner {
    async fn run(
        &self,
        bin: &str,
        args: &[&str],
        stdin: Option<&str>,
        _timeout_ms: u64,
    ) -> Result<Vec<u8>, String> {
        self.calls.lock().unwrap().push((
            bin.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
            stdin.map(|s| s.to_string()),
        ));
        self.results
            .get(bin)
            .cloned()
            .unwrap_or_else(|| Ok(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_session_prefers_wayland() {
        assert_eq!(
            detect_session(Some("wayland-1"), Some("tty"), None),
            Ok(Session::Wayland)
        );
    }

    #[test]
    fn detect_session_x11_via_session_type() {
        assert_eq!(detect_session(None, Some("x11"), None), Ok(Session::X11));
    }

    #[test]
    fn detect_session_x11_via_display_fallback() {
        assert_eq!(detect_session(None, None, Some(":0")), Ok(Session::X11));
    }

    #[test]
    fn detect_session_nothing_set_is_error() {
        let err = detect_session(None, Some("tty"), None).unwrap_err();
        assert!(err.contains("ERR_CLIPBOARD_NO_SESSION"), "{err}");
    }

    #[test]
    fn detect_session_empty_strings_treated_as_unset() {
        assert!(detect_session(Some("  "), None, None).is_err());
    }

    #[test]
    fn parse_provider_pref_variants() {
        assert_eq!(parse_provider_pref(None), Ok(ProviderPref::Auto));
        assert_eq!(parse_provider_pref(Some("")), Ok(ProviderPref::Auto));
        assert_eq!(parse_provider_pref(Some("auto")), Ok(ProviderPref::Auto));
        assert_eq!(
            parse_provider_pref(Some("wayland")),
            Ok(ProviderPref::Wayland)
        );
        assert_eq!(parse_provider_pref(Some("x11")), Ok(ProviderPref::X11));
        let err = parse_provider_pref(Some("macos")).unwrap_err();
        assert!(err.contains("ERR_CLIPBOARD_BAD_PARAMS"), "{err}");
    }

    #[test]
    fn wayland_chains_use_wl_tools() {
        let r = read_chain(Session::Wayland);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].bin, "wl-paste");
        assert_eq!(r[0].args, vec!["--no-newline"]);
        assert!(!r[0].stdin);

        let w = write_chain(Session::Wayland);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].bin, "wl-copy");
        assert!(w[0].stdin);
    }

    #[test]
    fn x11_chains_try_xclip_then_xsel() {
        let r = read_chain(Session::X11);
        assert_eq!(
            r.iter().map(|i| i.bin).collect::<Vec<_>>(),
            vec!["xclip", "xsel"]
        );
        assert!(!r.iter().any(|i| i.stdin));

        let w = write_chain(Session::X11);
        assert_eq!(
            w.iter().map(|i| i.bin).collect::<Vec<_>>(),
            vec!["xclip", "xsel"]
        );
        assert!(w.iter().all(|i| i.stdin));
        assert!(w[0].args.contains(&"-selection"));
    }

    #[tokio::test]
    async fn real_runner_missing_binary_maps_to_provider_missing() {
        let err = RealRunner
            .run("definitely-not-a-real-binary-xyz", &[], None, 1_000)
            .await
            .unwrap_err();
        assert!(err.contains("ERR_CLIPBOARD_PROVIDER_MISSING"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_runner_write_completes_when_child_daemonizes_holding_pipes() {
        let start = std::time::Instant::now();
        let out = RealRunner
            .run("sh", &["-c", "sleep 3 & exit 0"], Some("x"), 5_000)
            .await
            .expect("writer must complete on child exit, not pipe EOF");
        let elapsed = start.elapsed();
        assert!(out.is_empty());
        assert!(
            elapsed < std::time::Duration::from_millis(2_000),
            "write took {elapsed:?}; the daemonized `sleep 3` must not block it"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_runner_timeout_actually_bounds_a_stuck_child() {
        let start = std::time::Instant::now();
        let err = RealRunner
            .run("sh", &["-c", "sleep 30"], None, 300)
            .await
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(err.contains("ERR_CLIPBOARD_TIMEOUT"), "{err}");
        assert!(
            elapsed < std::time::Duration::from_millis(3_000),
            "timeout fired after {elapsed:?}; must fire near the 300ms budget"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_runner_writer_stderr_preserved_on_failure() {
        let err = RealRunner
            .run("sh", &["-c", "echo oops >&2; exit 7"], Some(""), 5_000)
            .await
            .unwrap_err();
        assert!(err.contains("exited with"), "{err}");
        assert!(err.contains("oops"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_runner_read_returns_stdout() {
        let out = RealRunner
            .run("sh", &["-c", "printf hello"], None, 5_000)
            .await
            .unwrap();
        assert_eq!(out, b"hello".to_vec());
    }
}
