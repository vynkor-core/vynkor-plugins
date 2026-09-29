//! The one artifact deployed on the phone: `hybcam-<sha8>.py` under
//! `<HOME>/<remote_dir>/`. The hash in the file name means a plugin upgrade
//! never runs a stale helper.

use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::PhoneError;
use crate::remote::{camera_env, RemoteCmd};
use crate::transport::Transport;

pub const SCRIPT: &str = include_str!("../helper/hybcam.py");

pub fn sha8() -> String {
    let d = Sha256::digest(SCRIPT.as_bytes());
    d.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

pub fn file_name() -> String {
    format!("hybcam-{}.py", sha8())
}

/// HOME-relative path of the current helper.
pub fn remote_path(cfg: &Config) -> String {
    format!("{}/{}", cfg.remote_dir, file_name())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperState {
    Ok,
    Stale,
    Missing,
}

impl HelperState {
    pub fn as_str(self) -> &'static str {
        match self {
            HelperState::Ok => "ok",
            HelperState::Stale => "stale",
            HelperState::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupResult {
    pub installed: bool,
    pub changed: bool,
    pub helper_path: String,
    pub sha8: String,
}

/// `python3 <helper> <args…>` with the camera environment, run from HOME.
pub fn helper_cmd(cfg: &Config, mut args: Vec<String>) -> RemoteCmd {
    args.insert(0, remote_path(cfg));
    RemoteCmd::new("python3", args).envs(camera_env(cfg.remote_uid)).in_home()
}

pub fn parse_state(listing: &str) -> HelperState {
    let current = file_name();
    let mut any_old = false;
    for line in listing.lines() {
        let l = line.trim();
        if l == current {
            return HelperState::Ok;
        }
        if l.starts_with("hybcam-") && l.ends_with(".py") {
            any_old = true;
        }
    }
    if any_old {
        HelperState::Stale
    } else {
        HelperState::Missing
    }
}

pub async fn check(t: &dyn Transport, cfg: &Config) -> Result<HelperState, PhoneError> {
    // `ls` of a missing dir prints nothing and exits nonzero; that is "missing", not an error.
    let cmd = RemoteCmd::new(
        "sh",
        vec!["-c".into(), "ls -1 \"$1\" 2>/dev/null; true".into(), "sh".into(), cfg.remote_dir.clone()],
    )
    .in_home();
    let o = t.run(&cmd, None, Duration::from_secs(5)).await?;
    if o.code == 255 {
        return Err(crate::error::classify(o.code, &o.stderr));
    }
    Ok(parse_state(&String::from_utf8_lossy(&o.stdout)))
}

pub async fn setup(t: &dyn Transport, cfg: &Config) -> Result<SetupResult, PhoneError> {
    let name = file_name();
    let result = |changed| SetupResult {
        installed: true,
        changed,
        helper_path: remote_path(cfg),
        sha8: sha8(),
    };
    if check(t, cfg).await? == HelperState::Ok {
        return Ok(result(false));
    }
    // atomic: write a dot-temp file, then rename; mkdir -p first
    let script = "mkdir -p \"$1\" && cat > \"$1/.$2.tmp\" && mv \"$1/.$2.tmp\" \"$1/$2\"";
    let cmd = RemoteCmd::new(
        "sh",
        vec!["-c".into(), script.into(), "sh".into(), cfg.remote_dir.clone(), name],
    )
    .in_home();
    let o = t.run(&cmd, Some(SCRIPT.as_bytes()), Duration::from_secs(15)).await?;
    if o.code != 0 {
        return Err(crate::error::classify(o.code, &o.stderr));
    }
    Ok(result(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::fake::FakeTransport;
    use crate::transport::Output;

    fn cfg() -> Config {
        Config::from_lookup(|k| match k {
            "HOME" => Some("/h".into()),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn hash_is_stable_eight_hex_and_in_the_file_name() {
        let s = sha8();
        assert_eq!(s.len(), 8);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(file_name(), format!("hybcam-{s}.py"));
        assert_eq!(remote_path(&cfg()), format!(".local/share/vyn-phone/hybcam-{s}.py"));
    }

    #[test]
    fn parse_state_distinguishes_ok_stale_missing() {
        assert_eq!(parse_state(&format!("x\n{}\n", file_name())), HelperState::Ok);
        assert_eq!(parse_state("hybcam-00000000.py\nother\n"), HelperState::Stale);
        assert_eq!(parse_state(""), HelperState::Missing);
        assert_eq!(parse_state("readme\n"), HelperState::Missing);
    }

    #[test]
    fn helper_cmd_runs_from_home_with_camera_env() {
        let c = helper_cmd(&cfg(), vec!["--cam".into(), "back".into()]);
        assert_eq!(c.program, "python3");
        assert_eq!(c.args[0], remote_path(&cfg()));
        assert_eq!(&c.args[1..], ["--cam", "back"]);
        assert!(c.in_home);
        assert!(c.env.iter().any(|(k, v)| k == "XDG_RUNTIME_DIR" && v == "/run/user/32011"));
        assert!(c.env.iter().any(|(k, _)| k == "MIR_SOCKET"));
    }

    #[tokio::test]
    async fn setup_is_idempotent_when_the_helper_is_already_there() {
        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(format!("{}\n", file_name()).into_bytes()));
        let r = setup(&t, &cfg()).await.unwrap();
        assert!(r.installed && !r.changed);
        assert_eq!(t.calls.lock().unwrap().len(), 1, "no deploy when already current");
    }

    #[tokio::test]
    async fn setup_deploys_the_embedded_script_over_stdin() {
        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(b"hybcam-00000000.py\n".to_vec())); // stale
        t.push_run(FakeTransport::ok(Vec::new())); // deploy
        let r = setup(&t, &cfg()).await.unwrap();
        assert!(r.installed && r.changed);
        let stdins = t.stdins.lock().unwrap();
        assert_eq!(stdins[1].as_deref(), Some(SCRIPT.as_bytes()));
        let calls = t.calls.lock().unwrap();
        assert!(calls[1].args.contains(&file_name()));
    }

    #[tokio::test]
    async fn setup_surfaces_a_failed_deploy_and_unreachable() {
        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(Vec::new()));
        t.push_run(Ok(Output { code: 1, stdout: vec![], stderr: "read-only file system".into() }));
        assert!(matches!(setup(&t, &cfg()).await, Err(PhoneError::Backend(_))));

        let t = FakeTransport::new();
        t.push_run(Ok(Output { code: 255, stdout: vec![], stderr: "no route".into() }));
        assert!(matches!(setup(&t, &cfg()).await, Err(PhoneError::Unreachable(_))));
    }

    #[test]
    fn embedded_helper_is_valid_python() {
        // Skipped silently where python3 is absent (CI without it).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h.py");
        std::fs::write(&p, SCRIPT).unwrap();
        match std::process::Command::new("python3").args(["-m", "py_compile"]).arg(&p).output() {
            Ok(o) => assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr)),
            Err(_) => eprintln!("python3 not found; skipping"),
        }
    }
}
