//! Environment configuration (`PHONE_PLUGIN_*`). `from_lookup` takes a lookup
//! closure so tests never mutate the process environment.

use std::path::PathBuf;

use crate::error::PhoneError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Ssh,
    Local,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub transport: TransportKind,
    pub ssh_host: String,
    /// ssh ControlMaster multiplexing (default on; `PHONE_PLUGIN_SSH_MUX=0` disables).
    pub ssh_mux: bool,
    pub remote_uid: u32,
    /// Helper directory, relative to the phone's HOME.
    pub remote_dir: String,
    /// Local data dir: photos, `latest.jpg`, records, the ssh control socket.
    pub dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self, PhoneError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, PhoneError> {
        let val = |k: &str| get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        let transport = match val("PHONE_PLUGIN_TRANSPORT").as_deref() {
            None | Some("ssh") => TransportKind::Ssh,
            Some("local") => TransportKind::Local,
            Some(other) => {
                return Err(PhoneError::BadParams(format!(
                    "PHONE_PLUGIN_TRANSPORT must be 'ssh' or 'local', got '{other}'"
                )))
            }
        };
        let ssh_host = val("PHONE_PLUGIN_SSH_HOST").unwrap_or_else(|| "mi6".to_string());
        validate_host(&ssh_host)?;
        let ssh_mux = val("PHONE_PLUGIN_SSH_MUX").as_deref() != Some("0");
        let remote_uid = match val("PHONE_PLUGIN_REMOTE_UID") {
            None => 32011,
            Some(s) => s
                .parse::<u32>()
                .map_err(|_| PhoneError::BadParams(format!("PHONE_PLUGIN_REMOTE_UID not a uid: '{s}'")))?,
        };
        let remote_dir = val("PHONE_PLUGIN_REMOTE_DIR").unwrap_or_else(|| ".local/share/vyn-phone".to_string());
        validate_remote_dir(&remote_dir)?;
        let dir = match val("PHONE_PLUGIN_DIR") {
            Some(d) => PathBuf::from(d),
            None => {
                let home = get("HOME").unwrap_or_else(|| "/tmp".to_string());
                PathBuf::from(home).join(".local/share/vyn/phone")
            }
        };
        Ok(Self { transport, ssh_host, ssh_mux, remote_uid, remote_dir, dir })
    }

    /// Create the data dir (mode 0700: it holds camera frames). Idempotent.
    pub fn ensure_dir(&self) -> Result<(), PhoneError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| PhoneError::Backend(format!("create {}: {e}", self.dir.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
}

pub fn validate_host(h: &str) -> Result<(), PhoneError> {
    let ok = !h.is_empty()
        && h.len() <= 253
        && !h.starts_with('-')
        && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(PhoneError::BadParams(format!("PHONE_PLUGIN_SSH_HOST invalid: '{h}'")))
    }
}

/// Relative path, components `[A-Za-z0-9._-]+`, no `..`, no leading `-`.
pub fn validate_remote_dir(d: &str) -> Result<(), PhoneError> {
    let bad = |why: &str| PhoneError::BadParams(format!("PHONE_PLUGIN_REMOTE_DIR '{d}': {why}"));
    if d.is_empty() || d.starts_with('/') {
        return Err(bad("must be a non-empty path relative to the phone's HOME"));
    }
    for comp in d.split('/') {
        if comp.is_empty() || comp == ".." || comp == "." || comp.starts_with('-') {
            return Err(bad("empty, '.', '..' or '-'-leading component"));
        }
        if !comp.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
            return Err(bad("only [A-Za-z0-9._-] allowed in components"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, PhoneError> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(move |k| m.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = cfg(&[("HOME", "/home/u")]).unwrap();
        assert_eq!(c.transport, TransportKind::Ssh);
        assert_eq!(c.ssh_host, "mi6");
        assert!(c.ssh_mux);
        assert_eq!(c.remote_uid, 32011);
        assert_eq!(c.remote_dir, ".local/share/vyn-phone");
        assert_eq!(c.dir, PathBuf::from("/home/u/.local/share/vyn/phone"));
    }

    #[test]
    fn overrides() {
        let c = cfg(&[
            ("PHONE_PLUGIN_TRANSPORT", "local"),
            ("PHONE_PLUGIN_SSH_HOST", "phone.lan"),
            ("PHONE_PLUGIN_SSH_MUX", "0"),
            ("PHONE_PLUGIN_REMOTE_UID", "1000"),
            ("PHONE_PLUGIN_REMOTE_DIR", "bin/vp"),
            ("PHONE_PLUGIN_DIR", "/data/p"),
        ])
        .unwrap();
        assert_eq!(c.transport, TransportKind::Local);
        assert_eq!(c.ssh_host, "phone.lan");
        assert!(!c.ssh_mux);
        assert_eq!(c.remote_uid, 1000);
        assert_eq!(c.remote_dir, "bin/vp");
        assert_eq!(c.dir, PathBuf::from("/data/p"));
    }

    #[test]
    fn rejects_bad_transport_host_uid_and_dir() {
        assert!(cfg(&[("PHONE_PLUGIN_TRANSPORT", "telnet")]).is_err());
        for h in ["-oProxyCommand=x", "a b", "a;b", "a$(x)", "h/../x"] {
            assert!(cfg(&[("PHONE_PLUGIN_SSH_HOST", h)]).is_err(), "host {h:?} must be rejected");
        }
        // an empty value means "unset" -> the default host, not an error
        assert_eq!(cfg(&[("PHONE_PLUGIN_SSH_HOST", "  ")]).unwrap().ssh_host, "mi6");
        assert!(cfg(&[("PHONE_PLUGIN_REMOTE_UID", "-1")]).is_err());
        assert!(cfg(&[("PHONE_PLUGIN_REMOTE_UID", "abc")]).is_err());
        for d in ["/abs", "../up", "a/../b", "a//b", "a b", "a;b", "-x", "a/-x", "."] {
            assert!(cfg(&[("PHONE_PLUGIN_REMOTE_DIR", d)]).is_err(), "dir {d:?} must be rejected");
        }
    }

    #[test]
    fn ensure_dir_creates_private_dir() {
        let t = tempfile::tempdir().unwrap();
        let c = cfg(&[("PHONE_PLUGIN_DIR", t.path().join("x/y").to_str().unwrap())]).unwrap();
        c.ensure_dir().unwrap();
        c.ensure_dir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&c.dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }
}
