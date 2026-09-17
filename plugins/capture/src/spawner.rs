//! Process execution boundary. Every screenshot/record/OCR backend spawns
//! a host binary directly with argv — never a shell — through this trait,
//! so tests run against [`FakeSpawner`] and never touch a real binary.

use std::io::ErrorKind;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::{Child, Command};

#[async_trait]
pub trait Process: Send + std::fmt::Debug {
    fn pid(&self) -> Option<u32>;
    fn start_kill(&mut self);
    fn try_wait(&mut self) -> Option<i32>;
}

pub type BoxedProcess = Box<dyn Process>;

#[async_trait]
pub trait Spawner: Send + Sync {
    /// Spawn `bin args` detached (stdio discarded); caller owns the
    /// returned handle's lifetime (records, long-running captures).
    async fn run_detached(&self, bin: &str, args: &[String]) -> Result<BoxedProcess, String>;

    /// Spawn `bin args`, wait for exit, and capture stdout as UTF-8. Used
    /// for short commands whose output the caller needs (`slurp`, `slop`,
    /// `tesseract ... stdout`).
    async fn run_capturing(&self, bin: &str, args: &[String]) -> Result<(i32, String), String>;
}

fn not_found_or_spawn_failed(bin: &str, e: std::io::Error) -> String {
    if e.kind() == ErrorKind::NotFound {
        format!("ERR_CAPTURE_PROVIDER_MISSING: binary '{bin}' not found on PATH")
    } else {
        format!("ERR_CAPTURE_BACKEND: spawn '{bin}' failed: {e}")
    }
}

pub struct RealSpawner;

#[async_trait]
impl Spawner for RealSpawner {
    async fn run_detached(&self, bin: &str, args: &[String]) -> Result<BoxedProcess, String> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match cmd.spawn() {
            Ok(child) => Ok(Box::new(RealProcess { child })),
            Err(e) => Err(not_found_or_spawn_failed(bin, e)),
        }
    }

    async fn run_capturing(&self, bin: &str, args: &[String]) -> Result<(i32, String), String> {
        let mut cmd = Command::new(bin);
        cmd.args(args).stdin(Stdio::null());
        let output = cmd.output().await.map_err(|e| not_found_or_spawn_failed(bin, e))?;
        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((code, stdout))
    }
}

#[derive(Debug)]
struct RealProcess {
    child: Child,
}

#[async_trait]
impl Process for RealProcess {
    fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    fn start_kill(&mut self) {
        let _ = self.child.start_kill();
    }

    fn try_wait(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }
}

#[cfg(test)]
pub use fake::FakeSpawner;

#[cfg(test)]
mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    pub struct FakeSpawner {
        /// Per-binary outcome for `run_detached`; unlisted binaries "not found".
        pub detached_ok: StdMutex<HashMap<String, bool>>,
        /// Per-binary `(exit_code, stdout)` for `run_capturing`; unlisted
        /// binaries "not found".
        pub capturing: StdMutex<HashMap<String, (i32, String)>>,
        pub detached_calls: StdMutex<Vec<(String, Vec<String>)>>,
        pub capturing_calls: StdMutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeSpawner {
        pub fn new() -> Self {
            Self {
                detached_ok: StdMutex::new(HashMap::new()),
                capturing: StdMutex::new(HashMap::new()),
                detached_calls: StdMutex::new(Vec::new()),
                capturing_calls: StdMutex::new(Vec::new()),
            }
        }

        pub fn allow_detached(&self, bin: &str) {
            self.detached_ok.lock().unwrap().insert(bin.to_string(), true);
        }

        pub fn set_capturing(&self, bin: &str, code: i32, stdout: &str) {
            self.capturing.lock().unwrap().insert(bin.to_string(), (code, stdout.to_string()));
        }
    }

    #[derive(Debug)]
    struct FakeProcess {
        exited: bool,
    }

    #[async_trait]
    impl Process for FakeProcess {
        fn pid(&self) -> Option<u32> {
            Some(1)
        }
        fn start_kill(&mut self) {
            self.exited = true;
        }
        fn try_wait(&mut self) -> Option<i32> {
            if self.exited {
                Some(0)
            } else {
                None
            }
        }
    }

    #[async_trait]
    impl Spawner for FakeSpawner {
        async fn run_detached(&self, bin: &str, args: &[String]) -> Result<BoxedProcess, String> {
            self.detached_calls.lock().unwrap().push((bin.to_string(), args.to_vec()));
            if self.detached_ok.lock().unwrap().get(bin).copied().unwrap_or(false) {
                Ok(Box::new(FakeProcess { exited: false }))
            } else {
                Err(format!("ERR_CAPTURE_PROVIDER_MISSING: binary '{bin}' not found on PATH"))
            }
        }

        async fn run_capturing(&self, bin: &str, args: &[String]) -> Result<(i32, String), String> {
            self.capturing_calls.lock().unwrap().push((bin.to_string(), args.to_vec()));
            self.capturing
                .lock()
                .unwrap()
                .get(bin)
                .cloned()
                .ok_or_else(|| format!("ERR_CAPTURE_PROVIDER_MISSING: binary '{bin}' not found on PATH"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn real_spawner_reports_missing_binary() {
        let sp = RealSpawner;
        let err = sp.run_detached("definitely-not-a-real-binary-xyz123", &[]).await.unwrap_err();
        assert!(err.contains("ERR_CAPTURE_PROVIDER_MISSING"), "{err}");
    }

    #[tokio::test]
    async fn fake_spawner_falls_through_on_unlisted_binary() {
        let sp = FakeSpawner::new();
        sp.allow_detached("grim");
        let err = sp.run_detached("gnome-screenshot", &[]).await.unwrap_err();
        assert!(err.contains("ERR_CAPTURE_PROVIDER_MISSING"), "{err}");
        assert!(sp.run_detached("grim", &[]).await.is_ok());
    }
}
