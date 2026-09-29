//! Typed error taxonomy. Every error carries a stable `ERR_PHONE_*` prefix so
//! callers can branch on the code without parsing prose (same convention as
//! `capture`'s `ERR_CAPTURE_*`).

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PhoneError {
    #[error("ERR_PHONE_BAD_PARAMS: {0}")]
    BadParams(String),
    #[error("ERR_PHONE_UNREACHABLE: {0}")]
    Unreachable(String),
    #[error("ERR_PHONE_BUSY: {0}")]
    Busy(String),
    #[error("ERR_PHONE_HELPER_MISSING: run phone_setup ({0})")]
    HelperMissing(String),
    #[error("ERR_PHONE_CAMERA: {0}")]
    Camera(String),
    #[error("ERR_PHONE_BACKEND: {0}")]
    Backend(String),
}

/// Last `max` characters of `s`, trimmed. Bounds how much remote stderr can
/// leak into an error message.
pub fn tail(s: &str, max: usize) -> String {
    let t = s.trim();
    let n = t.chars().count();
    if n <= max {
        t.to_string()
    } else {
        t.chars().skip(n - max).collect()
    }
}

/// Map a finished remote command (exit code + stderr) to the error taxonomy.
pub fn classify(code: i32, stderr: &str) -> PhoneError {
    let t = tail(stderr, 400);
    if code == 255 {
        return PhoneError::Unreachable(t);
    }
    if stderr.contains("No such file or directory") && stderr.contains("hybcam") {
        return PhoneError::HelperMissing(t);
    }
    if stderr.contains("connect FAILED") || stderr.contains("no frames") {
        return PhoneError::Camera(t);
    }
    PhoneError::Backend(format!("exit {code}: {t}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_exit_255_is_unreachable() {
        assert!(matches!(classify(255, "ssh: connect to host mi6: No route"), PhoneError::Unreachable(_)));
    }

    #[test]
    fn missing_helper_is_named_and_actionable() {
        let e = classify(2, "python3: can't open file 'x/hybcam-ab12cd34.py': [Errno 2] No such file or directory");
        assert!(matches!(e, PhoneError::HelperMissing(_)));
        assert!(e.to_string().contains("run phone_setup"));
    }

    #[test]
    fn camera_held_by_another_process_is_a_camera_error() {
        let e = classify(2, "connect FAILED");
        assert!(matches!(e, PhoneError::Camera(_)));
        assert!(e.to_string().starts_with("ERR_PHONE_CAMERA"));
    }

    #[test]
    fn no_frames_is_a_camera_error() {
        assert!(matches!(classify(3, "no frames"), PhoneError::Camera(_)));
    }

    #[test]
    fn anything_else_is_backend_with_code() {
        let e = classify(1, "boom");
        assert_eq!(e, PhoneError::Backend("exit 1: boom".into()));
    }

    #[test]
    fn stderr_tail_is_bounded() {
        let long = "x".repeat(10_000);
        let e = classify(1, &long);
        assert!(e.to_string().len() < 500, "unbounded stderr leaked: {}", e.to_string().len());
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("  ab  ", 10), "ab");
    }
}
