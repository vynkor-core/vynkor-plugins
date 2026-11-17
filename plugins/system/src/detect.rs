//! Runtime backend detection: probe the host once at startup and fill
//! [`SystemBackends`] with what actually exists. Everything undetected
//! stays `None` → `ERR_SYS_NOT_SUPPORTED` at call time.
//!
//! Probes are cheap one-shot calls (`wpctl --version`, one UPower property
//! read, one `pmset -g batt`) made before registration; the serve loop
//! never pays for them.
//!
//! The system D-Bus handle is wrapped in an opaque [`SystemBus`] so no
//! signature outside the `cfg(linux)` blocks mentions `zbus` — the crate
//! must keep compiling on macOS where that dependency doesn't exist.

use std::sync::Arc;

use crate::backends::{SystemBackends, Volume};
use crate::brightness;
use crate::runner::{CommandRunner, RealRunner, RunnerError, SharedRunner};
use crate::volume::{PactlVolume, WpctlVolume};

/// Detect every capability. Never fails: missing pieces become `None`.
pub async fn detect() -> SystemBackends {
    let runner: SharedRunner = Arc::new(RealRunner);
    // One shared system-bus connection for every system-bus capability.
    let sys_conn = SystemBus::connect().await;
    SystemBackends {
        battery: detect_battery(&runner, sys_conn.as_ref()).await,
        volume: detect_volume(Arc::clone(&runner)).await,
        brightness: brightness::detect(Arc::clone(&runner)),
        lock: detect_lock(&runner, sys_conn.as_ref()).await,
        power: detect_power(sys_conn.as_ref()).await,
    }
}

/// Opaque system-bus handle; `None` degrades every system-bus capability
/// independently and keeps zbus out of cross-platform signatures.
#[cfg(target_os = "linux")]
pub struct SystemBus(zbus::Connection);

#[cfg(not(target_os = "linux"))]
pub struct SystemBus;

impl SystemBus {
    async fn connect() -> Option<Self> {
        system_bus_connect().await
    }
}

#[cfg(target_os = "linux")]
async fn system_bus_connect() -> Option<SystemBus> {
    zbus::Connection::system().await.ok().map(SystemBus)
}

#[cfg(not(target_os = "linux"))]
async fn system_bus_connect() -> Option<SystemBus> {
    None
}

/// Battery: UPower DisplayDevice on Linux; one live `pmset -g batt`
/// probe on other platforms (desktop Macs without a battery fail the
/// probe → NOT_SUPPORTED, same semantics as UPower absence).
#[cfg(target_os = "linux")]
async fn detect_battery(
    _runner: &SharedRunner,
    bus: Option<&SystemBus>,
) -> Option<Arc<dyn crate::backends::Battery>> {
    let conn = &bus?.0;
    match crate::upower::UpowerBattery::connect(conn.clone()).await {
        Ok(b) => Some(Arc::new(b)),
        // No UPower on this host (headless server, container) — fine.
        Err(_) => None,
    }
}

#[cfg(not(target_os = "linux"))]
async fn detect_battery(
    runner: &SharedRunner,
    _bus: Option<&SystemBus>,
) -> Option<Arc<dyn crate::backends::Battery>> {
    let b = crate::macos::MacosBattery::new(Arc::clone(runner));
    match b.status().await {
        Ok(_) => Some(Arc::new(b)),
        Err(_) => None,
    }
}

/// Volume provider selection: `wpctl` (PipeWire) preferred, `pactl`
/// (PulseAudio/pipewire-pulse) fallback, `osascript` on macOS. A provider
/// is "present" when its binary exists and answers its probe; a
/// present-but-broken tool is still selected so callers see its real
/// error instead of silent absence.
async fn detect_volume(runner: SharedRunner) -> Option<Arc<dyn Volume>> {
    if probe(&*runner, "wpctl", &["--version"]).await {
        return Some(Arc::new(WpctlVolume::new(runner)));
    }
    if probe(&*runner, "pactl", &["--version"]).await {
        return Some(Arc::new(PactlVolume::new(runner)));
    }
    #[cfg(not(target_os = "linux"))]
    if probe(&*runner, "osascript", &["-e", "1"]).await {
        return Some(Arc::new(crate::macos::MacosVolume::new(runner)));
    }
    None
}

async fn probe(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> bool {
    !matches!(
        runner.run(program, args).await,
        Err(RunnerError::NotFound(_))
    )
}

/// Session lock: ScreenSaver → loginctl chain on Linux (always present —
/// neither path has a cheap presence probe); CGSession suspend on macOS.
#[cfg(target_os = "linux")]
async fn detect_lock(
    runner: &SharedRunner,
    bus: Option<&SystemBus>,
) -> Option<Arc<dyn crate::backends::SessionLock>> {
    let conn = bus?.0.clone();
    Some(Arc::new(crate::lock::SessionBusLock::new(
        conn,
        Arc::clone(runner),
    )))
}

#[cfg(not(target_os = "linux"))]
async fn detect_lock(
    runner: &SharedRunner,
    _bus: Option<&SystemBus>,
) -> Option<Arc<dyn crate::backends::SessionLock>> {
    Some(Arc::new(crate::macos::MacosLock::new(Arc::clone(runner))))
}

#[cfg(target_os = "linux")]
async fn detect_power(bus: Option<&SystemBus>) -> Option<Arc<dyn crate::backends::PowerProfiles>> {
    crate::power_profile::PpdProfiles::connect(&bus?.0)
        .await
        .ok()
        .map(|p| Arc::new(p) as Arc<dyn crate::backends::PowerProfiles>)
}

#[cfg(not(target_os = "linux"))]
async fn detect_power(_bus: Option<&SystemBus>) -> Option<Arc<dyn crate::backends::PowerProfiles>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{RunOutcome, RunResult};
    use std::collections::HashSet;
    use std::sync::Mutex;

    struct FakeRunner {
        /// Programs that "exist" on this fake PATH.
        present: HashSet<&'static str>,
        log: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn new(present: &[&'static str]) -> Self {
            Self {
                present: present.iter().copied().collect(),
                log: Mutex::new(Vec::new()),
            }
        }

        fn ran(&self) -> Vec<String> {
            self.log.lock().expect("log lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(&self, program: &str, _args: &[&str]) -> RunResult {
            self.log.lock().expect("log lock").push(program.to_string());
            if self.present.contains(program) {
                Ok(RunOutcome {
                    ok: true,
                    stdout: "version 1\n".into(),
                    stderr: String::new(),
                })
            } else {
                Err(RunnerError::NotFound(program.to_string()))
            }
        }
    }

    #[tokio::test]
    async fn prefers_wpctl_when_present() {
        let runner = Arc::new(FakeRunner::new(&["wpctl", "pactl"]));
        let vol = detect_volume(Arc::clone(&runner) as SharedRunner).await;
        assert!(vol.is_some());
        // Only wpctl was probed; pactl never needed.
        assert_eq!(runner.ran(), vec!["wpctl".to_string()]);
    }

    #[tokio::test]
    async fn falls_back_to_pactl_without_wpctl() {
        let runner = Arc::new(FakeRunner::new(&["pactl"]));
        let vol = detect_volume(Arc::clone(&runner) as SharedRunner).await;
        assert!(vol.is_some());
        assert_eq!(runner.ran(), vec!["wpctl".to_string(), "pactl".to_string()]);
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn falls_back_to_osascript_without_unix_audio_tools() {
        let runner = Arc::new(FakeRunner::new(&["osascript"]));
        let vol = detect_volume(Arc::clone(&runner) as SharedRunner).await;
        assert!(vol.is_some());
    }

    #[tokio::test]
    async fn no_provider_without_any_tool() {
        let runner = Arc::new(FakeRunner::new(&[]));
        let vol = detect_volume(Arc::clone(&runner) as SharedRunner).await;
        assert!(vol.is_none());
    }
}
