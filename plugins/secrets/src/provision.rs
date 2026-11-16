//! Operator-side vault writes, behind the `secrets-provision` binary.
//!
//! Vaults are per caller, so `secret_set` over the bus only ever writes the
//! caller's own vault: an operator calling it fills the operator's vault,
//! never the one `email` or `mqtt` reads with `secret_get`. This writes
//! `{data_dir}/{plugin_id}.vault` directly, with the master key and format
//! the plugin itself uses.
//!
//! The running plugin caches a vault once its caller has touched it. After
//! provisioning, restart `secrets`: otherwise it keeps serving the cached
//! copy, and its next write for that caller would drop this value.

use std::path::Path;

use crate::handler::sanitize_caller_id;
use crate::request::{valid_name, DEFAULT_MAX_NAME_BYTES};
use crate::vault::{Vault, VaultResult};

/// Store `name = value` in `plugin_id`'s vault, creating the vault if needed.
pub fn provision(
    data_dir: &Path,
    key: &[u8; 32],
    plugin_id: &str,
    name: &str,
    value: String,
) -> VaultResult<()> {
    let plugin_id = sanitize_caller_id(plugin_id)?;
    valid_name(name, DEFAULT_MAX_NAME_BYTES)?;
    if value.is_empty() {
        return Err("refusing to store an empty value".into());
    }
    let mut vault = Vault::load_or_create(data_dir.join(format!("{plugin_id}.vault")), key)?;
    vault.insert(name, value);
    vault.persist(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::Handler;
    use crate::request::DEFAULT_MAX_VALUE_BYTES;

    const KEY: [u8; 32] = [7u8; 32];

    #[tokio::test]
    async fn provisioned_secret_is_what_that_plugin_reads() {
        let dir = tempfile::tempdir().unwrap();
        provision(
            dir.path(),
            &KEY,
            "mqtt",
            "broker_password",
            "pw".into(),
        )
        .unwrap();

        let h = Handler::new(
            dir.path().to_path_buf(),
            KEY,
            DEFAULT_MAX_NAME_BYTES,
            DEFAULT_MAX_VALUE_BYTES,
        );
        let get = br#"{"name":"broker_password"}"#;
        let out = h.handle("mqtt", "secret_get", get).await.unwrap();
        assert_eq!(out, br#"{"found":true,"value":"pw"}"#);
        // Isolation holds: another caller does not see it.
        let out = h.handle("email", "secret_get", get).await.unwrap();
        assert_eq!(out, br#"{"found":false}"#);
    }

    #[test]
    fn keeps_existing_secrets_and_overwrites_the_named_one() {
        let dir = tempfile::tempdir().unwrap();
        provision(dir.path(), &KEY, "ai", "A", "1".into()).unwrap();
        provision(dir.path(), &KEY, "ai", "B", "2".into()).unwrap();
        provision(dir.path(), &KEY, "ai", "A", "3".into()).unwrap();
        let vault = Vault::load_or_create(dir.path().join("ai.vault"), &KEY).unwrap();
        assert_eq!(vault.get("A"), Some("3"));
        assert_eq!(vault.get("B"), Some("2"));
    }

    #[test]
    fn rejects_unsafe_ids_names_and_empty_values() {
        let dir = tempfile::tempdir().unwrap();
        assert!(provision(dir.path(), &KEY, "../x", "N", "v".into()).is_err());
        assert!(provision(dir.path(), &KEY, "mqtt", "bad name", "v".into()).is_err());
        assert!(provision(dir.path(), &KEY, "mqtt", "N", String::new()).is_err());
    }

    #[test]
    fn wrong_key_fails_instead_of_resetting_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        provision(dir.path(), &KEY, "mqtt", "A", "1".into()).unwrap();
        assert!(provision(dir.path(), &[8u8; 32], "mqtt", "B", "2".into()).is_err());
        let vault = Vault::load_or_create(dir.path().join("mqtt.vault"), &KEY).unwrap();
        assert_eq!(vault.get("A"), Some("1"));
    }
}
