//! Typed error taxonomy for the `system` plugin.
//!
//! Every error carries a stable `ERR_SYS_*` code prefix (same convention as
//! `media`'s `ERR_MEDIA_*`), so callers can branch on the code without
//! parsing prose. The `Display` rendering is what lands in
//! `ActionResponse.error`.

/// Stable error codes, mirrored in README's "Error taxonomy" table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysErrorCode {
    BadParams,
    NotFound,
    NotSupported,
    Backend,
}

impl SysErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            SysErrorCode::BadParams => "ERR_SYS_BAD_PARAMS",
            SysErrorCode::NotFound => "ERR_SYS_NOT_FOUND",
            SysErrorCode::NotSupported => "ERR_SYS_NOT_SUPPORTED",
            SysErrorCode::Backend => "ERR_SYS_BACKEND",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    /// Request params failed validation. Detail names the offending field.
    #[error("ERR_SYS_BAD_PARAMS: {0}")]
    BadParams(String),

    /// Action does not exist in this plugin's manifest.
    #[error("ERR_SYS_NOT_FOUND: unknown action '{0}'")]
    UnknownAction(String),

    /// The capability exists but no backend for it was detected on this
    /// host (no UPower, no audio server, non-Linux in P1, ...). Detail
    /// names the missing capability.
    #[error("ERR_SYS_NOT_SUPPORTED: {0} is not available on this system")]
    NotSupported(&'static str),

    /// A detected backend failed at call time (D-Bus error, spawn failure,
    /// unparseable tool output). Detail carries the cause; nothing here is
    /// sensitive — only tool output and interface names.
    #[error("ERR_SYS_BACKEND: {0}")]
    Backend(String),
}

impl SystemError {
    /// Machine-readable code for this error, stable across versions.
    pub const fn code(&self) -> SysErrorCode {
        match self {
            SystemError::BadParams(_) => SysErrorCode::BadParams,
            SystemError::UnknownAction(_) => SysErrorCode::NotFound,
            SystemError::NotSupported(_) => SysErrorCode::NotSupported,
            SystemError::Backend(_) => SysErrorCode::Backend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_per_variant() {
        assert_eq!(
            SystemError::BadParams("x".into()).code().as_str(),
            "ERR_SYS_BAD_PARAMS"
        );
        assert_eq!(
            SystemError::UnknownAction("nope".into()).code().as_str(),
            "ERR_SYS_NOT_FOUND"
        );
        assert_eq!(
            SystemError::NotSupported("battery").code().as_str(),
            "ERR_SYS_NOT_SUPPORTED"
        );
        assert_eq!(
            SystemError::Backend("boom".into()).code().as_str(),
            "ERR_SYS_BACKEND"
        );
    }

    #[test]
    fn display_carries_code_and_detail() {
        let e = SystemError::NotSupported("battery");
        assert_eq!(
            e.to_string(),
            "ERR_SYS_NOT_SUPPORTED: battery is not available on this system"
        );

        let e = SystemError::UnknownAction("foo".to_string());
        assert_eq!(e.to_string(), "ERR_SYS_NOT_FOUND: unknown action 'foo'");
    }
}
