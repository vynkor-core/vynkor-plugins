//! Screen backlight via sysfs, with a `brightnessctl` spawn fallback for
//! hosts where the plugin lacks write permission on the sysfs node.
//!
//! Safety contract: `set(0)` clamps to the device's minimum non-zero step
//! (target 1) — "as dark as allowed without blanking". A plugin must never
//! be able to strand the operator on a black screen; full blanking stays
//! with the keyboard's own controls.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backends::Brightness;
use crate::error::SystemError;
use crate::runner::CommandRunner;

const SYSFS_BASE: &str = "/sys/class/backlight";

pub struct SysfsBrightness {
    /// Device directory containing `max_brightness` / `brightness`.
    device_dir: PathBuf,
    runner: Arc<dyn CommandRunner>,
}

impl SysfsBrightness {
    pub fn new(device_dir: PathBuf, runner: Arc<dyn CommandRunner>) -> Self {
        Self { device_dir, runner }
    }

    async fn read_u64(&self, file: &str) -> Result<u64, SystemError> {
        let raw = tokio::fs::read_to_string(self.device_dir.join(file))
            .await
            .map_err(|e| SystemError::Backend(format!("read {file}: {e}")))?;
        raw.trim()
            .parse::<u64>()
            .map_err(|e| SystemError::Backend(format!("parse {file}: {e}")))
    }
}

#[async_trait::async_trait]
impl Brightness for SysfsBrightness {
    async fn get(&self) -> Result<u8, SystemError> {
        let cur = self.read_u64("brightness").await?;
        let max = self.read_u64("max_brightness").await?;
        calc_percent(cur, max).ok_or_else(|| SystemError::Backend("max_brightness is 0".into()))
    }

    async fn set(&self, percent: u8) -> Result<u8, SystemError> {
        let max = self.read_u64("max_brightness").await?;
        if max == 0 {
            return Err(SystemError::Backend("max_brightness is 0".into()));
        }
        match tokio::fs::write(
            self.device_dir.join("brightness"),
            target_raw(max, percent).to_string(),
        )
        .await
        {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                // No write access to the sysfs node — delegate to
                // brightnessctl, which operators install setuid/udev-capable.
                let pct = percent.to_string();
                let out = self
                    .runner
                    .run("brightnessctl", &["set", &format!("{pct}%")])
                    .await
                    .map_err(|e| SystemError::Backend(format!("brightnessctl failed: {e}")))?;
                if !out.ok {
                    return Err(SystemError::Backend(format!(
                        "brightnessctl exited nonzero: {}",
                        out.stderr.trim()
                    )));
                }
            }
            Err(e) => return Err(SystemError::Backend(format!("write brightness: {e}"))),
        }
        self.get().await
    }
}

/// First sysfs device exposing both control files, deterministic order.
pub fn detect_device_dir(base: &Path) -> Option<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(base)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    entries
        .into_iter()
        .find(|p| p.join("max_brightness").is_file() && p.join("brightness").is_file())
}

pub fn detect(runner: Arc<dyn CommandRunner>) -> Option<Arc<dyn Brightness>> {
    detect_device_dir(Path::new(SYSFS_BASE))
        .map(|dir| Arc::new(SysfsBrightness::new(dir, runner)) as Arc<dyn Brightness>)
}

/// Raw sysfs value for a percent, clamped to the non-blanking floor of 1.
fn target_raw(max: u64, percent: u8) -> u64 {
    let raw = u128::from(max) * u128::from(percent) / 100;
    let raw = u64::try_from(raw).unwrap_or(u64::MAX);
    if raw == 0 {
        1
    } else {
        raw
    }
}

fn calc_percent(cur: u64, max: u64) -> Option<u8> {
    if max == 0 {
        return None;
    }
    Some((f64::from(cur as u32) / f64::from(max as u32) * 100.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{RunOutcome, RunResult, RunnerError};
    use std::sync::Mutex;

    fn tmp_device(name: &str, max: u64, cur: u64) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("system-plugin-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("max_brightness"), format!("{max}\n")).expect("max");
        std::fs::write(dir.join("brightness"), format!("{cur}\n")).expect("cur");
        dir
    }

    struct FakeRunner {
        fail_spawns: bool,
        log: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn ok() -> Self {
            Self {
                fail_spawns: false,
                log: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                fail_spawns: true,
                log: Mutex::new(Vec::new()),
            }
        }

        fn ran(&self) -> Vec<String> {
            self.log.lock().expect("log").clone()
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(&self, program: &str, args: &[&str]) -> RunResult {
            self.log
                .lock()
                .expect("log")
                .push(format!("{program} {}", args.join(" ")));
            if self.fail_spawns {
                return Err(RunnerError::NotFound(program.to_string()));
            }
            Ok(RunOutcome {
                ok: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn detect_finds_first_deterministic_device() {
        let base =
            std::env::temp_dir().join(format!("system-plugin-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("intel_backlight")).unwrap();
        std::fs::create_dir_all(base.join("acpi_video0")).unwrap();
        // acpi_video0 sorts first but lacks control files — skipped.
        std::fs::write(base.join("intel_backlight/max_brightness"), b"255").unwrap();
        std::fs::write(base.join("intel_backlight/brightness"), b"128").unwrap();
        assert_eq!(detect_device_dir(&base), Some(base.join("intel_backlight")));
        assert_eq!(detect_device_dir(&base.join("nonexistent")), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn target_clamps_at_nonblanking_floor() {
        assert_eq!(target_raw(100, 50), 50);
        assert_eq!(target_raw(255, 100), 255);
        assert_eq!(target_raw(255, 1), 2);
        assert_eq!(target_raw(255, 0), 1);
        assert_eq!(target_raw(0, 50), 1);
    }

    #[tokio::test]
    async fn get_and_set_roundtrip_on_real_tmp_files() {
        let dir = tmp_device("roundtrip", 2550, 1275);
        let b = SysfsBrightness::new(dir.clone(), Arc::new(FakeRunner::ok()));
        assert_eq!(b.get().await.unwrap(), 50);
        assert_eq!(b.set(80).await.unwrap(), 80);
        let written: u64 = std::fs::read_to_string(dir.join("brightness"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(written, 2040); // 2550 * 80 / 100
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn permission_denied_falls_back_to_brightnessctl() {
        let dir = tmp_device("fallback", 1000, 500);
        // Make the node read-only so the direct write hits EACCES.
        let mut perm = std::fs::metadata(dir.join("brightness"))
            .unwrap()
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o444);
        std::fs::set_permissions(dir.join("brightness"), perm).unwrap();

        let runner = Arc::new(FakeRunner::ok());
        let b = SysfsBrightness::new(dir.clone(), Arc::clone(&runner) as Arc<dyn CommandRunner>);
        b.set(30).await.expect("fallback path succeeds");
        assert_eq!(
            runner.ran(),
            vec!["brightnessctl set 30%".to_string()],
            "fallback must invoke brightnessctl with the percent form"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_brightnessctl_surfaces_backend_error() {
        let dir = tmp_device("nofallback", 1000, 500);
        let mut perm = std::fs::metadata(dir.join("brightness"))
            .unwrap()
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o444);
        std::fs::set_permissions(dir.join("brightness"), perm).unwrap();

        let runner = Arc::new(FakeRunner::failing());
        let b = SysfsBrightness::new(dir, runner);
        let e = b.set(30).await.unwrap_err();
        assert_eq!(e.code().as_str(), "ERR_SYS_BACKEND");
    }
}
