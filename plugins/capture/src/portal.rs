//! `org.freedesktop.portal.Screenshot` fallback — the universal path when
//! no direct binary was found for the session (unknown/future DE,
//! sandboxed environment). Always interactive by construction (the portal
//! shows its own picker), same session-bus stack `hotkey`'s
//! `GlobalShortcuts` backend and `media`'s MPRIS watcher already use.

use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};
use zbus::{Connection, Proxy};

use crate::error::CaptureError;

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENSHOT_INTERFACE: &str = "org.freedesktop.portal.Screenshot";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

pub async fn screenshot_via_portal(interactive: bool, dest: &std::path::Path) -> Result<(), CaptureError> {
    let conn = Connection::session()
        .await
        .map_err(|e| CaptureError::Backend(format!("portal session bus: {e}")))?;
    let proxy = Proxy::new(&conn, PORTAL_SERVICE, PORTAL_PATH, SCREENSHOT_INTERFACE)
        .await
        .map_err(|e| CaptureError::Backend(format!("portal proxy: {e}")))?;

    let mut options: std::collections::HashMap<&str, ZValue> = std::collections::HashMap::new();
    options.insert("interactive", ZValue::from(interactive));
    options.insert("handle_token", ZValue::from("vynkor_capture"));

    let handle: OwnedObjectPath = proxy
        .call("Screenshot", &("", options))
        .await
        .map_err(|e| CaptureError::Backend(format!("Screenshot call: {e}")))?;

    let request = Proxy::new(&conn, PORTAL_SERVICE, handle.as_str(), REQUEST_INTERFACE)
        .await
        .map_err(|e| CaptureError::Backend(format!("request proxy: {e}")))?;
    let mut stream = request
        .receive_signal("Response")
        .await
        .map_err(|e| CaptureError::Backend(format!("subscribe Response: {e}")))?;
    let msg = tokio::time::timeout(std::time::Duration::from_secs(120), stream.next())
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
    let src_path = uri
        .strip_prefix("file://")
        .ok_or_else(|| CaptureError::Backend(format!("unexpected portal uri scheme: {uri}")))?;
    std::fs::copy(src_path, dest).map_err(|e| CaptureError::Backend(format!("copy portal output: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
}
