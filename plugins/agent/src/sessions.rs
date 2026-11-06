//! Chat-session glue between `agent` and the `chats` plugin: who is asking
//! (device / kind / source), which session the goal belongs to, prior turns
//! for the LLM context, and the finished turn written back.
//!
//! A turn never fails because of `chats`: every error here degrades to
//! "no session" and the goal runs exactly as it did before sessions existed.

use serde::{Deserialize, Serialize};

/// Where a goal came from. `device` and `source` are derived from the
/// kernel-stamped `caller_plugin_id`, which a plugin cannot forge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub device: String,
    pub kind: String,
    pub source: String,
}

/// Derive the origin of a goal.
///
/// - `caller` = `<device>.<plugin>` (relayed from a device): device is the
///   part before the first dot, source defaults to the plugin part;
/// - `caller` without a dot (local plugin) or empty: device is
///   `local_device`, source defaults to the caller (`"local"` when empty);
/// - `kind` defaults to `voice`.
pub fn origin(
    caller: &str,
    kind: Option<&str>,
    source: Option<&str>,
    local_device: &str,
) -> Origin {
    let (device, default_source) = match caller.split_once('.') {
        Some((dev, plugin)) if !dev.is_empty() => (dev.to_string(), plugin.to_string()),
        _ if caller.is_empty() => (local_device.to_string(), "local".to_string()),
        _ => (local_device.to_string(), caller.to_string()),
    };
    Origin {
        device,
        kind: kind.unwrap_or("voice").to_string(),
        source: source.map(str::to_string).unwrap_or(default_source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(device: &str, kind: &str, source: &str) -> Origin {
        Origin { device: device.into(), kind: kind.into(), source: source.into() }
    }

    #[test]
    fn origin_from_relayed_device_caller() {
        assert_eq!(origin("laptop.desktop", None, None, "host"), o("laptop", "voice", "desktop"));
    }

    #[test]
    fn origin_from_local_plugin_caller() {
        assert_eq!(origin("telegram", Some("chat"), None, "host"), o("host", "chat", "telegram"));
    }

    #[test]
    fn origin_from_empty_caller_uses_explicit_source() {
        assert_eq!(origin("", None, Some("ctl"), "mbp"), o("mbp", "voice", "ctl"));
        assert_eq!(origin("", None, None, "mbp"), o("mbp", "voice", "local"));
    }
}
