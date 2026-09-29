//! A remote command as *data* (program + argv + env), never a shell string.
//! Only [`RemoteCmd::to_shell`] renders it for a remote shell, quoting every
//! token, so no caller-supplied text can change the command's structure.

use crate::error::PhoneError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCmd {
    pub env: Vec<(String, String)>,
    pub program: String,
    pub args: Vec<String>,
    /// Run with the phone user's HOME as the working directory (helper paths
    /// are HOME-relative).
    pub in_home: bool,
}

/// POSIX single-quote quoting: `'` becomes `'\''`. The result is one word.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn valid_env_key(k: &str) -> bool {
    let mut cs = k.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

impl RemoteCmd {
    pub fn new(program: &str, args: Vec<String>) -> Self {
        Self { env: Vec::new(), program: program.to_string(), args, in_home: false }
    }

    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.to_string(), v.to_string()));
        self
    }

    pub fn envs(mut self, kv: Vec<(String, String)>) -> Self {
        self.env.extend(kv);
        self
    }

    pub fn in_home(mut self) -> Self {
        self.in_home = true;
        self
    }

    /// Render for a POSIX remote shell: `[cd "$HOME" && ]K='v' … 'prog' 'a' 'b'`.
    pub fn to_shell(&self) -> Result<String, PhoneError> {
        let has_nul = |s: &str| s.contains('\0');
        if has_nul(&self.program) || self.args.iter().any(|a| has_nul(a)) || self.env.iter().any(|(_, v)| has_nul(v)) {
            return Err(PhoneError::BadParams("NUL byte in remote command".into()));
        }
        let mut out = String::new();
        if self.in_home {
            out.push_str("cd \"$HOME\" && ");
        }
        for (k, v) in &self.env {
            if !valid_env_key(k) {
                return Err(PhoneError::BadParams(format!("invalid env name '{k}'")));
            }
            out.push_str(k);
            out.push('=');
            out.push_str(&shell_quote(v));
            out.push(' ');
        }
        out.push_str(&shell_quote(&self.program));
        for a in &self.args {
            out.push(' ');
            out.push_str(&shell_quote(a));
        }
        Ok(out)
    }
}

/// Environment the camera helper needs on the phone (same as `qmlscene`).
pub fn camera_env(uid: u32) -> Vec<(String, String)> {
    vec![
        ("XDG_RUNTIME_DIR".to_string(), format!("/run/user/{uid}")),
        ("MIR_SOCKET".to_string(), format!("/run/user/{uid}/mir_socket_trusted")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn renders_a_plain_command() {
        let c = RemoteCmd::new("python3", vec!["a b".into(), "-W".into(), "640".into()]).env("K", "v").in_home();
        assert_eq!(c.to_shell().unwrap(), "cd \"$HOME\" && K='v' 'python3' 'a b' '-W' '640'");
    }

    #[test]
    fn quote_handles_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn hostile_tokens_survive_a_real_shell_as_literal_data() {
        let hostile = [
            "plain",
            "with space",
            "quote'inside",
            "dq\"inside",
            "semi;colon",
            "amp&&echo pwned",
            "$(echo pwned)",
            "`echo pwned`",
            "back\\slash",
            "new\nline",
            "-leading-dash",
            "*glob*",
            "$HOME",
            "a|b>c<d",
            "",
        ];
        for h in hostile {
            let cmd = RemoteCmd::new("printf", vec!["%s".into(), h.to_string()]);
            let out = Command::new("sh").arg("-c").arg(cmd.to_shell().unwrap()).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), h, "token {h:?} was interpreted");
            assert!(out.status.success());
        }
    }

    #[test]
    fn hostile_env_values_survive_a_real_shell() {
        let cmd = RemoteCmd::new("sh", vec!["-c".into(), "printf %s \"$V\"".into()]).env("V", "a'b;$(echo pwned) c");
        let out = Command::new("sh").arg("-c").arg(cmd.to_shell().unwrap()).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a'b;$(echo pwned) c");
    }

    #[test]
    fn invalid_env_names_and_nul_are_rejected() {
        for k in ["lower", "1X", "A-B", "A B", "A=B", "", "A;B"] {
            let c = RemoteCmd::new("true", vec![]).env(k, "v");
            assert!(c.to_shell().is_err(), "env name {k:?} must be rejected");
        }
        assert!(RemoteCmd::new("t\0rue", vec![]).to_shell().is_err());
        assert!(RemoteCmd::new("true", vec!["a\0b".into()]).to_shell().is_err());
    }

    #[test]
    fn in_home_changes_directory_first() {
        let cmd = RemoteCmd::new("pwd", vec![]).in_home();
        let out = Command::new("sh").arg("-c").arg(cmd.to_shell().unwrap()).env("HOME", "/tmp").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "/tmp");
    }

    #[test]
    fn camera_env_uses_the_uid() {
        let e = camera_env(32011);
        assert_eq!(e[0], ("XDG_RUNTIME_DIR".into(), "/run/user/32011".into()));
        assert_eq!(e[1], ("MIR_SOCKET".into(), "/run/user/32011/mir_socket_trusted".into()));
    }
}
