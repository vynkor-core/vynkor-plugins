//! Typed error taxonomy for the `capture` plugin. Every error carries a
//! stable `ERR_CAPTURE_*` code prefix (same convention as `system`'s
//! `ERR_SYS_*`), so callers can branch on the code without parsing prose.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureErrorCode {
    BadParams,
    NotSupported,
    Busy,
    Cancelled,
    Backend,
}

impl CaptureErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            CaptureErrorCode::BadParams => "ERR_CAPTURE_BAD_PARAMS",
            CaptureErrorCode::NotSupported => "ERR_CAPTURE_NOT_SUPPORTED",
            CaptureErrorCode::Busy => "ERR_CAPTURE_BUSY",
            CaptureErrorCode::Cancelled => "ERR_CAPTURE_CANCELLED",
            CaptureErrorCode::Backend => "ERR_CAPTURE_BACKEND",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("ERR_CAPTURE_BAD_PARAMS: {0}")]
    BadParams(String),

    /// No backend for this capability was found on the host after trying
    /// every candidate in the chain. Detail names the capability.
    #[error("ERR_CAPTURE_NOT_SUPPORTED: {0} is not available on this system")]
    NotSupported(&'static str),

    /// A recording is already in progress (single active slot).
    #[error("ERR_CAPTURE_BUSY: a recording is already in progress")]
    Busy,

    /// An interactive selection (slurp/slop/maim -s/import/gnome-screenshot
    /// -a/spectacle -r) was dismissed by the user. Detail names the tool.
    #[error("ERR_CAPTURE_CANCELLED: {0} selection was cancelled")]
    Cancelled(&'static str),

    /// A detected backend was spawned but failed (nonzero exit, unparseable
    /// output, D-Bus error). Detail carries the cause.
    #[error("ERR_CAPTURE_BACKEND: {0}")]
    Backend(String),
}

impl CaptureError {
    pub const fn code(&self) -> CaptureErrorCode {
        match self {
            CaptureError::BadParams(_) => CaptureErrorCode::BadParams,
            CaptureError::NotSupported(_) => CaptureErrorCode::NotSupported,
            CaptureError::Busy => CaptureErrorCode::Busy,
            CaptureError::Cancelled(_) => CaptureErrorCode::Cancelled,
            CaptureError::Backend(_) => CaptureErrorCode::Backend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_per_variant() {
        assert_eq!(CaptureError::BadParams("x".into()).code().as_str(), "ERR_CAPTURE_BAD_PARAMS");
        assert_eq!(CaptureError::NotSupported("record").code().as_str(), "ERR_CAPTURE_NOT_SUPPORTED");
        assert_eq!(CaptureError::Busy.code().as_str(), "ERR_CAPTURE_BUSY");
        assert_eq!(CaptureError::Cancelled("slurp").code().as_str(), "ERR_CAPTURE_CANCELLED");
        assert_eq!(CaptureError::Backend("boom".into()).code().as_str(), "ERR_CAPTURE_BACKEND");
    }

    #[test]
    fn display_carries_code_and_detail() {
        let e = CaptureError::NotSupported("record");
        assert_eq!(e.to_string(), "ERR_CAPTURE_NOT_SUPPORTED: record is not available on this system");
        let e = CaptureError::Cancelled("slurp");
        assert_eq!(e.to_string(), "ERR_CAPTURE_CANCELLED: slurp selection was cancelled");
    }
}
