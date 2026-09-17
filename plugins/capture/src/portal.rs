//! `org.freedesktop.portal.Screenshot` fallback — the universal path when
//! no direct binary was found for the session (unknown/future DE,
//! sandboxed environment). Always interactive by construction (the portal
//! shows its own picker), same session-bus stack `hotkey`'s
//! `GlobalShortcuts` backend and `media`'s MPRIS watcher already use.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};
use zbus::{Connection, Proxy};

use crate::error::CaptureError;

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENSHOT_INTERFACE: &str = "org.freedesktop.portal.Screenshot";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub async fn screenshot_via_portal(interactive: bool, dest: &std::path::Path) -> Result<(), CaptureError> {
    let conn = Connection::session()
        .await
        .map_err(|e| CaptureError::Backend(format!("portal session bus: {e}")))?;
    let proxy = Proxy::new(&conn, PORTAL_SERVICE, PORTAL_PATH, SCREENSHOT_INTERFACE)
        .await
        .map_err(|e| CaptureError::Backend(format!("portal proxy: {e}")))?;

    // Each call gets its own token so two concurrent `capture_screenshot`
    // calls (the SDK's action loop runs actions concurrently) never
    // collide on the same Request object path.
    let token = format!("vynkor_capture_{}", unix_millis());
    let unique = conn
        .unique_name()
        .ok_or_else(|| CaptureError::Backend("portal: no unique bus name".into()))?;
    let expected_path = request_path(unique.as_str(), &token);

    // Subscribe to the Request's Response signal BEFORE issuing the
    // Screenshot call: a portal that answers fast (e.g. `interactive:
    // false`, or an auto-approved permission) can emit Response before a
    // subscription created only after the call returns would exist,
    // which would misreport a real, prompt success as a 120s timeout /
    // ERR_CAPTURE_CANCELLED. See `hotkey`'s `portal.rs` for the same
    // predict-the-path-first pattern against a different portal
    // interface.
    let request_proxy = Proxy::new(&conn, PORTAL_SERVICE, expected_path.as_str(), REQUEST_INTERFACE)
        .await
        .map_err(|e| CaptureError::Backend(format!("request proxy: {e}")))?;
    let mut stream = request_proxy
        .receive_signal("Response")
        .await
        .map_err(|e| CaptureError::Backend(format!("subscribe Response: {e}")))?;

    let mut options: std::collections::HashMap<&str, ZValue> = std::collections::HashMap::new();
    options.insert("interactive", ZValue::from(interactive));
    options.insert("handle_token", ZValue::from(token.as_str()));

    let handle: OwnedObjectPath = proxy
        .call("Screenshot", &("", options))
        .await
        .map_err(|e| CaptureError::Backend(format!("Screenshot call: {e}")))?;

    // Some portal implementations route the reply to a server-chosen path
    // that doesn't match the documented convention; retarget the stream
    // when ours didn't predict it correctly.
    if handle.as_str() != expected_path {
        stream = Proxy::new(&conn, PORTAL_SERVICE, handle.as_str(), REQUEST_INTERFACE)
            .await
            .map_err(|e| CaptureError::Backend(format!("request proxy: {e}")))?
            .receive_signal("Response")
            .await
            .map_err(|e| CaptureError::Backend(format!("subscribe Response: {e}")))?;
    }

    let msg = tokio::time::timeout(RESPONSE_TIMEOUT, stream.next())
        .await
        .map_err(|_| CaptureError::Cancelled("portal"))?
        .ok_or_else(|| CaptureError::Backend("portal Response stream closed".into()))?;
    let body: (u32, std::collections::HashMap<String, OwnedValue>) = msg
        .body()
        .deserialize()
        .map_err(|e| CaptureError::Backend(format!("decode Response: {e}")))?;
    let (response_code, mut results) = body;
    if response_code != 0 {
        return Err(CaptureError::Cancelled("portal"));
    }
    let uri: String = results
        .remove("uri")
        .and_then(|v| String::try_from(v).ok())
        .ok_or_else(|| CaptureError::Backend("portal response missing 'uri'".into()))?;
    let src_path = uri_to_path(&uri)?;

    move_file(&src_path, dest)
        .map_err(|e| CaptureError::Backend(format!("move portal output: {e}")))?;
    Ok(())
}

/// Move the portal's output into `dest`, preferring a rename (no
/// leftover duplicate outside `CAPTURE_PLUGIN_DIR`) and falling back to
/// copy+remove only when the rename can't be done atomically because the
/// source and destination are on different filesystems (`EXDEV`) — the
/// one case `fs::rename` can't handle.
fn move_file(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            std::fs::copy(src, dest)?;
            std::fs::remove_file(src)?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Turn a portal-returned `file://...` URI into a filesystem path. XDG
/// portal URIs are percent-encoded (e.g. spaces become `%20`, as real
/// implementations like `xdg-desktop-portal-gnome` produce when writing
/// to `~/Pictures/Screenshots/Screenshot From ....png`), so the scheme
/// prefix can simply be stripped but the remainder must be percent-decoded
/// before use.
fn uri_to_path(uri: &str) -> Result<PathBuf, CaptureError> {
    let encoded = uri
        .strip_prefix("file://")
        .ok_or_else(|| CaptureError::Backend(format!("unexpected portal uri scheme: {uri}")))?;
    let bytes = percent_decode(encoded);
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

/// Decode `%XX` hex escapes into raw bytes; any other byte passes through
/// unchanged. No new dependency: this is the entire percent-decoding
/// algorithm needed for a portal file URI's path component.
fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// The XDG portal convention for the Request object path a call will
/// reply on, derivable before the call returns: the sender's unique bus
/// name with its leading `:` stripped and `.` replaced with `_`, plus the
/// `handle_token` used in the call's options.
/// <https://flatpak.github.io/xdg-desktop-portal/docs/index.html#gdbus-org.freedesktop.portal.Request>
fn request_path(unique_name: &str, token: &str) -> String {
    format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        unique_name.trim_start_matches(':').replace('.', "_"),
        token
    )
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_strips_scheme() {
        let uri = "file:///run/user/1000/xdg-desktop-portal/abc.png";
        assert_eq!(uri.strip_prefix("file://"), Some("/run/user/1000/xdg-desktop-portal/abc.png"));
    }

    #[test]
    fn non_file_uri_has_no_prefix_match() {
        let uri = "http://example.com/x.png";
        assert_eq!(uri.strip_prefix("file://"), None);
    }

    #[test]
    fn uri_to_path_percent_decodes_spaces() {
        let uri = "file:///home/user/Pictures/Screenshots/Screenshot%20From%202026-09-17.png";
        let got = uri_to_path(uri).unwrap();
        assert_eq!(got, PathBuf::from("/home/user/Pictures/Screenshots/Screenshot From 2026-09-17.png"));
    }

    #[test]
    fn uri_to_path_round_trips_plain_path_unchanged() {
        let uri = "file:///run/user/1000/xdg-desktop-portal/abc.png";
        let got = uri_to_path(uri).unwrap();
        assert_eq!(got, PathBuf::from("/run/user/1000/xdg-desktop-portal/abc.png"));
    }

    #[test]
    fn percent_decode_handles_mixed_case_hex() {
        assert_eq!(percent_decode("a%2Fb%2fc"), b"a/b/c".to_vec());
    }

    #[test]
    fn request_path_matches_xdg_convention() {
        assert_eq!(
            request_path(":1.204", "vynkor_capture_123"),
            "/org/freedesktop/portal/desktop/request/1_204/vynkor_capture_123"
        );
    }
}
