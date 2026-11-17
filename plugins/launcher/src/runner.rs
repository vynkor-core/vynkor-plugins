use async_trait::async_trait;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, bin: &str, args: &[String], timeout_ms: u64) -> Result<String, String>;
}

pub struct RealRunner;

#[async_trait]
impl Runner for RealRunner {
    async fn run(&self, bin: &str, args: &[String], timeout_ms: u64) -> Result<String, String> {
        let mut cmd = Command::new(bin);
        cmd.args(args).kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("ERR_LAUNCH_PROVIDER_MISSING: binary '{bin}' not found on PATH")
            } else {
                format!("ERR_LAUNCH_SPAWN_FAILED: spawn '{bin}' failed: {e}")
            }
        })?;
        let res = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await;
        match res {
            Err(_) => {
                let _ = child.kill().await;
                Err(format!(
                    "ERR_LAUNCH_TIMEOUT: '{bin}' exceeded {timeout_ms}ms"
                ))
            }
            Ok(Err(e)) => Err(format!("ERR_LAUNCH_SPAWN_FAILED: wait '{bin}' failed: {e}")),
            Ok(Ok(status)) => {
                if !status.success() {
                    Err(format!(
                        "ERR_LAUNCH_SPAWN_FAILED: '{bin}' exited with {status}"
                    ))
                } else {
                    Ok(String::new())
                }
            }
        }
    }
}

/// For launching, we detach: spawn and return immediately without waiting for exit.
/// But for dry testing we can keep wait. The real launch uses fire-and-forget spawn.

#[async_trait]
pub trait Launcher: Send + Sync {
    async fn spawn_detached(&self, bin: &str, args: &[String]) -> Result<(), String>;
}

pub struct RealLauncher;

/// gui/session env a delegated app needs; a transient service runs with the
/// user manager's limits (DefaultLimitAS=infinity) but NOT our env, so
/// forward these explicitly
const FORWARD_ENV: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "GDK_BACKEND",
];

/// argv prefix for systemd-run up to (excluding) `--`.
///
/// transient service, never `--scope`: scope execs the target from THIS
/// process, so it inherits our kernel-capped RLIMIT_AS (soft=hard, not
/// raisable unprivileged) and big-VA apps like firefox die on mmap
/// (launched:true yet no window). a service is forked by the user manager
/// with DefaultLimitAS=infinity instead.
///
/// KillMode=process: the default control-group sweep kills the launched app
/// the moment the delegating launcher (gtk-launch/gio/xdg-open) exits —
/// the unit's main pid IS the launcher, not the app.
fn systemd_sargs(delegating: bool) -> Vec<String> {
    let mut sargs = vec![
        "--user".to_string(),
        "--collect".to_string(),
        "--property=KillMode=process".to_string(),
    ];
    if delegating {
        // wire the unit's stdio to ours so wait_with_output sees errors
        sargs.push("--pipe".to_string());
    }
    for key in FORWARD_ENV {
        if let Some(val) = std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .and_then(|v| v.into_string().ok())
        {
            sargs.push(format!("--setenv={key}={val}"));
        }
    }
    sargs
}

fn spawn_err(bin: &str) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("ERR_LAUNCH_PROVIDER_MISSING: binary '{bin}' not found on PATH")
        } else {
            format!("ERR_LAUNCH_SPAWN_FAILED: spawn '{bin}' failed: {e}")
        }
    }
}

/// first 400 chars of captured output as ERR_LAUNCH_FAILED detail
fn failed_detail(tool: &str, bin: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        output.status.to_string()
    };
    let detail = detail.chars().take(400).collect::<String>();
    format!(
        "ERR_LAUNCH_FAILED: '{tool} {bin}' exited {}: {}",
        output.status, detail
    )
}

#[async_trait]
impl Launcher for RealLauncher {
    async fn spawn_detached(&self, bin: &str, args: &[String]) -> Result<(), String> {
        let use_systemd = binary_in_path("systemd-run");
        let delegating = matches!(bin, "gtk-launch" | "gio" | "xdg-open");
        if use_systemd {
            let mut sargs = systemd_sargs(delegating);
            sargs.push("--".to_string());
            sargs.push(bin.to_string());
            sargs.extend_from_slice(args);
            let mut cmd = Command::new("systemd-run");
            cmd.args(&sargs).kill_on_drop(false);
            if delegating {
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let child = cmd.spawn().map_err(spawn_err("systemd-run"))?;
                // systemd-run exits right after queuing the unit while the
                // app inside lives on for hours — a plain wait would close
                // the pipe readers under it (EPIPE). drain in a task that
                // owns the child until the whole process tree exits, and
                // surface the queue result through a oneshot.
                let (tx, rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let _ = tx.send(child.wait_with_output().await);
                });
                match tokio::time::timeout(Duration::from_secs(5), rx).await {
                    // queue slower than the window, or drain task vanished
                    // without reporting — assume the handoff happened
                    Err(_) | Ok(Err(_)) => {}
                    Ok(Ok(Err(e))) => {
                        return Err(format!(
                            "ERR_LAUNCH_SPAWN_FAILED: wait 'systemd-run' failed: {e}"
                        ))
                    }
                    Ok(Ok(Ok(output))) => {
                        if !output.status.success() {
                            return Err(failed_detail("systemd-run", bin, &output));
                        }
                    }
                }
            } else {
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                cmd.spawn().map_err(spawn_err("systemd-run"))?;
            }
            Ok(())
        } else {
            if delegating {
                let mut cmd = Command::new(bin);
                cmd.args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(false);
                unsafe {
                    cmd.as_std_mut().pre_exec(|| {
                        libc::setsid();
                        let lim = libc::rlimit {
                            rlim_cur: libc::RLIM_INFINITY,
                            rlim_max: libc::RLIM_INFINITY,
                        };
                        libc::setrlimit(libc::RLIMIT_AS, &lim);
                        Ok(())
                    });
                }
                let child = cmd.spawn().map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        format!("ERR_LAUNCH_PROVIDER_MISSING: binary '{bin}' not found on PATH")
                    } else {
                        format!("ERR_LAUNCH_SPAWN_FAILED: spawn '{bin}' failed: {e}")
                    }
                })?;
                let output =
                    match tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
                        .await
                    {
                        Ok(r) => r.map_err(|e| {
                            format!("ERR_LAUNCH_SPAWN_FAILED: wait '{bin}' failed: {e}")
                        })?,
                        Err(_) => return Ok(()),
                    };
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    let detail = if !stderr.is_empty() {
                        stderr
                    } else if !stdout.is_empty() {
                        stdout
                    } else {
                        output.status.to_string()
                    };
                    let detail = detail.chars().take(400).collect::<String>();
                    return Err(format!(
                        "ERR_LAUNCH_FAILED: '{bin}' exited {}: {}",
                        output.status, detail
                    ));
                }
                Ok(())
            } else {
                let mut cmd = Command::new(bin);
                cmd.args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(false);
                unsafe {
                    cmd.as_std_mut().pre_exec(|| {
                        libc::setsid();
                        let lim = libc::rlimit {
                            rlim_cur: libc::RLIM_INFINITY,
                            rlim_max: libc::RLIM_INFINITY,
                        };
                        libc::setrlimit(libc::RLIMIT_AS, &lim);
                        Ok(())
                    });
                }
                cmd.spawn().map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        format!("ERR_LAUNCH_PROVIDER_MISSING: binary '{bin}' not found on PATH")
                    } else {
                        format!("ERR_LAUNCH_SPAWN_FAILED: spawn '{bin}' failed: {e}")
                    }
                })?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod sargs_tests {
    use super::*;

    #[test]
    fn delegating_includes_pipe_and_forwards_present_env() {
        std::env::set_var("WAYLAND_DISPLAY", "wayland-test");
        std::env::remove_var("DISPLAY");
        let sargs = systemd_sargs(true);
        assert_eq!(sargs[0], "--user");
        assert!(sargs.contains(&"--collect".to_string()));
        assert!(sargs.contains(&"--property=KillMode=process".to_string()));
        assert!(sargs.contains(&"--pipe".to_string()));
        assert!(sargs.contains(&"--setenv=WAYLAND_DISPLAY=wayland-test".to_string()));
        assert!(!sargs.iter().any(|a| a.starts_with("--setenv=DISPLAY=")));
    }

    #[test]
    fn plain_omits_pipe_and_empty_env_vars() {
        std::env::set_var("XAUTHORITY", "");
        let sargs = systemd_sargs(false);
        assert!(!sargs.contains(&"--pipe".to_string()));
        assert!(!sargs.iter().any(|a| a.starts_with("--setenv=XAUTHORITY=")));
        // nothing after the setenv block except caller-appended -- bin args
        assert!(sargs.iter().all(|a| a.starts_with("--")));
    }
}

#[cfg(test)]
pub struct FakeLauncher {
    pub calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    pub result: Result<(), String>,
}

#[cfg(test)]
impl FakeLauncher {
    pub fn new_ok() -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            result: Ok(()),
        }
    }
    pub fn new_err(msg: &str) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            result: Err(msg.to_string()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl Launcher for FakeLauncher {
    async fn spawn_detached(&self, bin: &str, args: &[String]) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push((bin.to_string(), args.to_vec()));
        self.result.clone()
    }
}

#[cfg(test)]
pub struct FakeRunner {
    pub calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
}

#[cfg(test)]
#[async_trait]
impl Runner for FakeRunner {
    async fn run(&self, bin: &str, args: &[String], _timeout_ms: u64) -> Result<String, String> {
        self.calls
            .lock()
            .unwrap()
            .push((bin.to_string(), args.to_vec()));
        Ok(String::new())
    }
}

pub fn binary_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file() && is_executable(&candidate)
            })
        })
        .unwrap_or(false)
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}
