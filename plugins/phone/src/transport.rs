//! Process-execution boundary. `SshTransport` wraps a [`RemoteCmd`] in `ssh`;
//! `LocalTransport` runs it directly (kernel on the phone); `fake::FakeTransport`
//! is the test double. Same shape as `capture`'s `Spawner`, extended with
//! binary stdout, stdin and long-running children.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::config::{Config, TransportKind};
use crate::error::PhoneError;
use crate::remote::RemoteCmd;

#[derive(Debug, Clone)]
pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

#[async_trait]
pub trait Control: Send {
    /// Kill the child and reap it. Idempotent.
    async fn kill(&mut self);
    async fn wait(&mut self) -> Option<i32>;
    /// Last few KiB of the child's stderr.
    fn stderr_tail(&self) -> String;
}

pub struct Spawned {
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    pub control: Box<dyn Control>,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn run(&self, cmd: &RemoteCmd, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, PhoneError>;
    async fn spawn(&self, cmd: &RemoteCmd) -> Result<Spawned, PhoneError>;
    /// Short label for status output: `ssh:mi6` or `local`.
    fn describe(&self) -> String;
}

/// Build the transport the config asks for.
pub fn from_config(cfg: &Config) -> Arc<dyn Transport> {
    match cfg.transport {
        TransportKind::Ssh => Arc::new(SshTransport::new(&cfg.ssh_host, cfg.ssh_mux, &cfg.dir)),
        TransportKind::Local => Arc::new(LocalTransport::new()),
    }
}

/// A resolved host command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

pub struct LocalTransport;

impl LocalTransport {
    pub fn new() -> Self {
        Self
    }

    pub fn exec(cmd: &RemoteCmd) -> Exec {
        Exec {
            program: cmd.program.clone(),
            args: cmd.args.clone(),
            env: cmd.env.clone(),
            cwd: if cmd.in_home { std::env::var_os("HOME").map(PathBuf::from) } else { None },
        }
    }
}

impl Default for LocalTransport {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SshTransport {
    host: String,
    mux: bool,
    control_dir: PathBuf,
}

/// The exact `ssh` argv for one remote command. Key auth only (`BatchMode`),
/// no host-key policy override, optional ControlMaster multiplexing.
pub fn ssh_args(host: &str, mux: bool, control_dir: &Path, remote_shell: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"].iter().map(|s| s.to_string()).collect();
    if mux {
        a.extend([
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            "ControlPersist=60".to_string(),
            "-o".to_string(),
            format!("ControlPath={}/cm-%C", control_dir.display()),
        ]);
    }
    a.push(host.to_string());
    a.push("--".to_string());
    a.push(remote_shell.to_string());
    a
}

impl SshTransport {
    pub fn new(host: &str, mux: bool, control_dir: &Path) -> Self {
        Self { host: host.to_string(), mux, control_dir: control_dir.to_path_buf() }
    }

    pub fn exec(&self, cmd: &RemoteCmd) -> Result<Exec, PhoneError> {
        let shell = cmd.to_shell()?;
        Ok(Exec {
            program: "ssh".to_string(),
            args: ssh_args(&self.host, self.mux, &self.control_dir, &shell),
            env: Vec::new(),
            cwd: None,
        })
    }
}

fn command(e: &Exec) -> Command {
    let mut c = Command::new(&e.program);
    c.args(&e.args).envs(e.env.iter().map(|(k, v)| (k, v))).kill_on_drop(true);
    if let Some(d) = &e.cwd {
        c.current_dir(d);
    }
    c
}

fn spawn_err(program: &str, e: std::io::Error) -> PhoneError {
    if e.kind() == std::io::ErrorKind::NotFound {
        PhoneError::Backend(format!("binary '{program}' not found on PATH"))
    } else {
        PhoneError::Backend(format!("spawn '{program}' failed: {e}"))
    }
}

async fn run_exec(e: &Exec, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, PhoneError> {
    let mut c = command(e);
    c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|err| spawn_err(&e.program, err))?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let data = data.to_vec();
        tokio::spawn(async move {
            let _ = pipe.write_all(&data).await;
            let _ = pipe.shutdown().await;
        });
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => Ok(Output {
            code: o.status.code().unwrap_or(-1),
            stdout: o.stdout,
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
        Ok(Err(err)) => Err(PhoneError::Backend(format!("wait '{}' failed: {err}", e.program))),
        Err(_) => Err(PhoneError::Unreachable(format!("timed out after {}s", timeout.as_secs()))),
    }
}

struct ChildControl {
    child: Child,
    stderr: Arc<Mutex<Vec<u8>>>,
}

const STDERR_KEEP: usize = 4096;

#[async_trait]
impl Control for ChildControl {
    async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
    async fn wait(&mut self) -> Option<i32> {
        self.child.wait().await.ok().and_then(|s| s.code())
    }
    fn stderr_tail(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }
}

async fn spawn_exec(e: &Exec) -> Result<Spawned, PhoneError> {
    let mut c = command(e);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|err| spawn_err(&e.program, err))?;
    let stdout = child.stdout.take().ok_or_else(|| PhoneError::Backend("child stdout missing".into()))?;
    let mut stderr = child.stderr.take().ok_or_else(|| PhoneError::Backend("child stderr missing".into()))?;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buf);
    tokio::spawn(async move {
        let mut chunk = [0u8; 512];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut b = sink.lock().unwrap();
                    b.extend_from_slice(&chunk[..n]);
                    if b.len() > STDERR_KEEP {
                        let cut = b.len() - STDERR_KEEP;
                        b.drain(..cut);
                    }
                }
            }
        }
    });
    Ok(Spawned { stdout: Box::new(stdout), control: Box::new(ChildControl { child, stderr: buf }) })
}

#[async_trait]
impl Transport for LocalTransport {
    async fn run(&self, cmd: &RemoteCmd, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, PhoneError> {
        run_exec(&Self::exec(cmd), stdin, timeout).await
    }
    async fn spawn(&self, cmd: &RemoteCmd) -> Result<Spawned, PhoneError> {
        spawn_exec(&Self::exec(cmd)).await
    }
    fn describe(&self) -> String {
        "local".to_string()
    }
}

#[async_trait]
impl Transport for SshTransport {
    async fn run(&self, cmd: &RemoteCmd, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, PhoneError> {
        run_exec(&self.exec(cmd)?, stdin, timeout).await
    }
    async fn spawn(&self, cmd: &RemoteCmd) -> Result<Spawned, PhoneError> {
        spawn_exec(&self.exec(cmd)?).await
    }
    fn describe(&self) -> String {
        format!("ssh:{}", self.host)
    }
}

/// Test double used by unit tests here and the fake-kernel test in `main.rs`.
pub mod fake {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;

    pub struct FakeTransport {
        pub calls: Mutex<Vec<RemoteCmd>>,
        pub stdins: Mutex<Vec<Option<Vec<u8>>>>,
        runs: Mutex<VecDeque<Result<Output, PhoneError>>>,
        stream_bytes: Mutex<Vec<u8>>,
        hold_open: bool,
        pub killed: Arc<AtomicBool>,
        delay: Duration,
        active: AtomicUsize,
        pub max_active: AtomicUsize,
    }

    impl FakeTransport {
        pub fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                stdins: Mutex::new(Vec::new()),
                runs: Mutex::new(VecDeque::new()),
                stream_bytes: Mutex::new(Vec::new()),
                hold_open: false,
                killed: Arc::new(AtomicBool::new(false)),
                delay: Duration::ZERO,
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
            }
        }
        pub fn push_run(&self, r: Result<Output, PhoneError>) {
            self.runs.lock().unwrap().push_back(r);
        }
        pub fn ok(stdout: Vec<u8>) -> Result<Output, PhoneError> {
            Ok(Output { code: 0, stdout, stderr: String::new() })
        }
        /// Bytes the next `spawn` emits on stdout; `hold_open` keeps the pipe
        /// open (a live stream) until `kill`.
        pub fn with_stream(mut self, bytes: Vec<u8>, hold_open: bool) -> Self {
            *self.stream_bytes.lock().unwrap() = bytes;
            self.hold_open = hold_open;
            self
        }
        pub fn with_delay(mut self, d: Duration) -> Self {
            self.delay = d;
            self
        }
    }

    impl Default for FakeTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    struct FakeControl {
        killed: Arc<AtomicBool>,
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl Control for FakeControl {
        async fn kill(&mut self) {
            self.killed.store(true, Ordering::SeqCst);
            self.notify.notify_one();
        }
        async fn wait(&mut self) -> Option<i32> {
            Some(0)
        }
        fn stderr_tail(&self) -> String {
            String::new()
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn run(&self, cmd: &RemoteCmd, stdin: Option<&[u8]>, _t: Duration) -> Result<Output, PhoneError> {
            self.calls.lock().unwrap().push(cmd.clone());
            self.stdins.lock().unwrap().push(stdin.map(|s| s.to_vec()));
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.runs
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(PhoneError::Backend("FakeTransport: no scripted run result".into())))
        }

        async fn spawn(&self, cmd: &RemoteCmd) -> Result<Spawned, PhoneError> {
            use tokio::io::AsyncWriteExt as _;
            self.calls.lock().unwrap().push(cmd.clone());
            let (mut w, r) = tokio::io::duplex(8 * 1024 * 1024);
            let bytes = self.stream_bytes.lock().unwrap().clone();
            let hold = self.hold_open;
            let notify = Arc::new(Notify::new());
            let n2 = Arc::clone(&notify);
            tokio::spawn(async move {
                let _ = w.write_all(&bytes).await;
                if hold {
                    n2.notified().await;
                }
                drop(w);
            });
            Ok(Spawned {
                stdout: Box::new(r),
                control: Box::new(FakeControl { killed: Arc::clone(&self.killed), notify }),
            })
        }

        fn describe(&self) -> String {
            "fake".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_argv_shape_with_and_without_mux() {
        let a = ssh_args("mi6", true, Path::new("/d"), "cd \"$HOME\" && 'x'");
        assert_eq!(&a[0..4], ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]);
        assert!(a.contains(&"ControlPath=/d/cm-%C".to_string()));
        assert!(a.contains(&"ControlPersist=60".to_string()));
        assert_eq!(a[a.len() - 3], "mi6");
        assert_eq!(a[a.len() - 2], "--");
        assert_eq!(a[a.len() - 1], "cd \"$HOME\" && 'x'");
        assert!(!a.iter().any(|s| s.contains("StrictHostKeyChecking")));

        let b = ssh_args("mi6", false, Path::new("/d"), "x");
        assert!(!b.iter().any(|s| s.contains("Control")));
    }

    #[test]
    fn ssh_exec_quotes_the_remote_command() {
        let t = SshTransport::new("mi6", false, Path::new("/d"));
        let e = t.exec(&RemoteCmd::new("python3", vec!["a b; rm -rf /".into()]).in_home()).unwrap();
        assert_eq!(e.program, "ssh");
        assert_eq!(e.args.last().unwrap(), "cd \"$HOME\" && 'python3' 'a b; rm -rf /'");
    }

    #[test]
    fn local_exec_uses_home_as_cwd_only_when_asked() {
        let plain = LocalTransport::exec(&RemoteCmd::new("true", vec![]));
        assert_eq!(plain.cwd, None);
        let home = LocalTransport::exec(&RemoteCmd::new("true", vec![]).in_home());
        assert_eq!(home.cwd, std::env::var_os("HOME").map(PathBuf::from));
    }

    #[tokio::test]
    async fn local_run_captures_binary_stdout_stdin_and_exit_code() {
        let t = LocalTransport::new();
        let o = t
            .run(&RemoteCmd::new("printf", vec!["%s".into(), "hi".into()]), None, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!((o.code, o.stdout.as_slice()), (0, &b"hi"[..]));

        let payload: Vec<u8> = (0u8..=255).collect();
        let o = t.run(&RemoteCmd::new("cat", vec![]), Some(&payload), Duration::from_secs(5)).await.unwrap();
        assert_eq!(o.stdout, payload, "binary stdin/stdout must round-trip");

        let o = t.run(&RemoteCmd::new("sh", vec!["-c".into(), "echo err >&2; exit 7".into()]), None, Duration::from_secs(5)).await.unwrap();
        assert_eq!(o.code, 7);
        assert_eq!(o.stderr.trim(), "err");
    }

    #[tokio::test]
    async fn local_run_times_out_and_missing_binary_is_reported() {
        let t = LocalTransport::new();
        let e = t.run(&RemoteCmd::new("sleep", vec!["5".into()]), None, Duration::from_millis(100)).await.unwrap_err();
        assert!(matches!(e, PhoneError::Unreachable(_)), "{e}");
        let e = t.run(&RemoteCmd::new("definitely-not-a-binary-xyz", vec![]), None, Duration::from_secs(1)).await.unwrap_err();
        assert!(matches!(e, PhoneError::Backend(_)) && e.to_string().contains("not found"), "{e}");
    }

    #[tokio::test]
    async fn local_spawn_streams_stdout_and_kill_is_idempotent() {
        let t = LocalTransport::new();
        let mut s = t
            .spawn(&RemoteCmd::new("sh", vec!["-c".into(), "printf abc; echo oops >&2; sleep 30".into()]))
            .await
            .unwrap();
        let mut buf = [0u8; 3];
        tokio::io::AsyncReadExt::read_exact(&mut s.stdout, &mut buf).await.unwrap();
        assert_eq!(&buf, b"abc");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(s.control.stderr_tail().contains("oops"));
        s.control.kill().await;
        s.control.kill().await;
    }
}
