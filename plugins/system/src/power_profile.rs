//! Power profile via power-profiles-daemon on the system bus.
//!
//! The daemon registers two name/path pairs over its history — the
//! original `net.hadess.PowerProfiles` + `/net/hadess/PowerProfiles` and
//! the renamed `org.freedesktop.UPower.PowerProfiles` +
//! `/org/freedesktop/UPower/PowerProfiles` — while keeping the D-Bus
//! interface (`net.hadess.PowerProfiles`) stable. Detection tries both
//! pairs and reports NOT_SUPPORTED when neither answers (TLP-only hosts,
//! servers).

use async_trait::async_trait;
use serde::Serialize;
use zbus::Connection;

use crate::error::SystemError;
use crate::request::Profile;

#[zbus::proxy(interface = "net.hadess.PowerProfiles", assume_defaults = true)]
trait PowerProfiles {
    #[zbus(property)]
    fn profile(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn set_profile(&self, value: String) -> zbus::Result<()>;

    #[zbus(property)]
    fn profiles(&self) -> zbus::Result<Vec<String>>;
}

/// Wire state of the active profile plus what the host offers.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProfileState {
    /// Active profile as the daemon spells it (our three canonical strings,
    /// or a raw value from a future daemon release).
    pub profile: String,
    pub available: Vec<String>,
}

pub struct PpdProfiles {
    proxy: PowerProfilesProxy<'static>,
}

impl PpdProfiles {
    /// Try both registered name/path pairs; `Err` when neither answers.
    pub async fn connect(conn: &Connection) -> Result<Self, SystemError> {
        for (service, path) in [
            (
                "org.freedesktop.UPower.PowerProfiles",
                "/org/freedesktop/UPower/PowerProfiles",
            ),
            ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
        ] {
            let built = PowerProfilesProxy::builder(conn)
                .destination(service)
                .and_then(|b| b.path(path));
            let Ok(proxy) = built else { continue };
            if let Ok(proxy) = proxy.build().await {
                // Probe: an object answering the interface must expose the
                // profiles list; anything else counts as absent.
                if proxy.profiles().await.is_ok() {
                    return Ok(Self { proxy });
                }
            }
        }
        Err(SystemError::NotSupported("power-profiles-daemon"))
    }
}

#[async_trait]
impl super::backends::PowerProfiles for PpdProfiles {
    async fn get(&self) -> Result<ProfileState, SystemError> {
        let profile = self
            .proxy
            .profile()
            .await
            .map_err(|e| SystemError::Backend(format!("ppd profile: {e}")))?;
        let available = self
            .proxy
            .profiles()
            .await
            .map_err(|e| SystemError::Backend(format!("ppd profiles: {e}")))?;
        Ok(ProfileState { profile, available })
    }

    async fn set(&self, profile: Profile) -> Result<ProfileState, SystemError> {
        self.proxy
            .set_profile(profile.as_str().to_string())
            .await
            .map_err(|e| SystemError::Backend(format!("ppd set: {e}")))?;
        self.get().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_state_serializes_wire_shape() {
        let s = ProfileState {
            profile: "balanced".into(),
            available: vec![
                "performance".into(),
                "balanced".into(),
                "power-saver".into(),
            ],
        };
        let v = serde_json::to_value(&s).expect("serialize");
        assert_eq!(v["profile"], "balanced");
        assert_eq!(v["available"].as_array().map(Vec::len), Some(3));
    }
}
