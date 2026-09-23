//! Argv-only process spawning behind a trait, so the audio-stream backends
//! are testable without `pw-dump`/`wpctl`/`pactl` on PATH (same boundary as
//! `system`'s `CommandRunner`).
//!
//! Commands are always `program + args`, never a shell, and every spawn is
//! bounded by [`RUN_TIMEOUT`] so a hung tool degrades to an error instead of
//! stalling the serve loop.

use std::time::Duration;

use async_trait::async_trait;

/// Per-spawn timeout for every host tool invocation.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of one spawned tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum RunnerError {
    /// The binary does not exist — backend detection falls through to the
    /// next provider on this.
    NotFound(String),
    Timeout(String),
    Io(String),
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunnerError::NotFound(p) => write!(f, "binary not found: {p}"),
            RunnerError::Timeout(p) => write!(f, "timeout running {p}"),
            RunnerError::Io(m) => write!(f, "{m}"),
        }
    }
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, program: &str, args: &[&str]) -> Result<RunOutcome, RunnerError>;
}

pub struct RealRunner;

#[async_trait]
impl CommandRunner for RealRunner {
    async fn run(&self, program: &str, args: &[&str]) -> Result<RunOutcome, RunnerError> {
        let output = tokio::time::timeout(
            RUN_TIMEOUT,
            tokio::process::Command::new(program).args(args).output(),
        )
        .await
        .map_err(|_| RunnerError::Timeout(program.to_string()))?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => RunnerError::NotFound(program.to_string()),
            _ => RunnerError::Io(format!("failed to run {program}: {e}")),
        })?;
        Ok(RunOutcome {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
