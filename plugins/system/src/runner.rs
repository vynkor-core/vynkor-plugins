//! Argv-only process spawning behind a trait, so volume backends are
//! testable without the real tools on PATH (same boundary as `clipboard`'s
//! `Runner`).
//!
//! Security model: commands are always `program + args`, never a shell —
//! tool output can never inject anything. Every spawn is bounded by
//! [`RUN_TIMEOUT`] so a hung backend degrades to an error instead of
//! stalling the serve loop.

use std::sync::Arc;
use std::time::Duration;

/// Per-spawn timeout for every host tool invocation.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of one spawned tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The binary does not exist (spawn returned `NotFound`). Volume
    /// detection uses this to fall through to the next provider.
    #[error("binary not found: {0}")]
    NotFound(String),

    /// Spawn succeeded but the tool did not finish within [`RUN_TIMEOUT`].
    #[error("timeout running {0}")]
    Timeout(String),

    /// Any other spawn/IO failure.
    #[error("failed to run {program}: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

pub type RunResult = Result<RunOutcome, RunnerError>;

/// Test seam over process spawning.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, program: &str, args: &[&str]) -> RunResult;
}

/// Real spawner: argv-only, no shell, bounded by [`RUN_TIMEOUT`].
pub struct RealRunner;

#[async_trait::async_trait]
impl CommandRunner for RealRunner {
    async fn run(&self, program: &str, args: &[&str]) -> RunResult {
        let output = tokio::time::timeout(
            RUN_TIMEOUT,
            tokio::process::Command::new(program).args(args).output(),
        )
        .await
        .map_err(|_| RunnerError::Timeout(program.to_string()))?
        .map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => RunnerError::NotFound(program.to_string()),
            _ => RunnerError::Io {
                program: program.to_string(),
                source,
            },
        })?;

        Ok(RunOutcome {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Convenience alias for shared runner handles.
pub type SharedRunner = Arc<dyn CommandRunner>;
