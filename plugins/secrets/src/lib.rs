//! Library crate for the `secrets` plugin.
//!
//! The `ConcurrentHandler` impl lives here (not in `main`) because of the
//! orphan rule: the trait comes from `vynkor-sdk`, the type from this crate.

pub mod handler;
pub mod provision;
pub mod request;
pub mod vault;

use std::sync::Arc;

use vynkor_sdk::concurrent::{response_envelope, ConcurrentHandler};
use vynkor_sdk::proto::{ActionRequest, Envelope, PluginManifest};
use vynkor_sdk::VynkorError;

pub const PLUGIN_ID: &str = "secrets";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

impl ConcurrentHandler for handler::Handler {
    fn id(&self) -> &str {
        PLUGIN_ID
    }

    fn version(&self) -> &str {
        PLUGIN_VERSION
    }

    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            permissions: vec!["PERMISSION_SECRETS".into()],
            actions: vec![
                "secret_set".into(),
                "secret_get".into(),
                "secret_delete".into(),
                "secret_list".into(),
            ],
            action_specs: vynkor_plugin_manifest::action_specs(),
            ..Default::default()
        }
    }

    async fn on_action(&self, req: ActionRequest) -> Vec<Envelope> {
        let result = self
            .handle(&req.caller_plugin_id, &req.action, &req.params_json)
            .await;
        vec![response_envelope(req.action_id, result)]
    }

    async fn on_shutdown(&self) -> Result<(), VynkorError> {
        Ok(())
    }
}

/// Build the plugin handler from environment configuration. Panics on
/// missing/invalid required config (same convention as `database`).
pub fn handler_from_env() -> Arc<handler::Handler> {
    // the kernel grants every plugin its own dir as VYN_DATA_DIR
    let data_dir = std::env::var("SECRETS_PLUGIN_DATA_DIR")
        .or_else(|_| std::env::var("VYN_DATA_DIR"))
        .unwrap_or_else(|_| {
            panic!("SECRETS_PLUGIN_DATA_DIR (or the kernel's VYN_DATA_DIR) must be set")
        });

    let master_key_raw = match std::env::var("SECRETS_PLUGIN_MASTER_KEY") {
        Ok(k) => zeroize::Zeroizing::new(k),
        Err(_) => stored_master_key(std::path::Path::new(&data_dir))
            .unwrap_or_else(|e| panic!("no SECRETS_PLUGIN_MASTER_KEY and {e}")),
    };
    let master_key = vault::parse_master_key(&master_key_raw)
        .unwrap_or_else(|e| panic!("SECRETS_PLUGIN_MASTER_KEY invalid: {e}"));

    let max_name_bytes = std::env::var("SECRETS_PLUGIN_MAX_NAME_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(request::DEFAULT_MAX_NAME_BYTES);
    let max_value_bytes = std::env::var("SECRETS_PLUGIN_MAX_VALUE_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(request::DEFAULT_MAX_VALUE_BYTES);

    Arc::new(handler::Handler::new(
        data_dir.into(),
        master_key,
        max_name_bytes,
        max_value_bytes,
    ))
}

/// `<data_dir>/master.key` (0600), created on first start: a plugin
/// installed from the store has no operator-supplied key. Weaker than a key
/// kept off this disk (set SECRETS_PLUGIN_MASTER_KEY for that), but the vault
/// still keeps values out of plain sight in its files and backups of them.
fn stored_master_key(data_dir: &std::path::Path) -> Result<zeroize::Zeroizing<String>, String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = data_dir.join("master.key");
    if let Ok(k) = std::fs::read_to_string(&path) {
        return Ok(zeroize::Zeroizing::new(k));
    }
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
    let mut raw = zeroize::Zeroizing::new([0u8; 32]);
    getrandom::fill(raw.as_mut()).map_err(|e| format!("no randomness for a master key: {e}"))?;
    let hex = zeroize::Zeroizing::new(raw.iter().map(|b| format!("{b:02x}")).collect::<String>());
    // create_new: two starts racing must not overwrite each other's key
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(hex.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            Ok(hex)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => std::fs::read_to_string(&path)
            .map(zeroize::Zeroizing::new)
            .map_err(|e| format!("cannot read {}: {e}", path.display())),
        Err(e) => Err(format!("cannot create {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod stored_key_tests {
    use super::stored_master_key;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn a_key_is_made_once_private_and_then_reused() {
        let d = tempfile::tempdir().unwrap();
        let k1 = stored_master_key(d.path()).unwrap();
        assert!(crate::vault::parse_master_key(&k1).is_ok());
        let mode = std::fs::metadata(d.path().join("master.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(*stored_master_key(d.path()).unwrap(), *k1);
    }
}

#[cfg(test)]
mod manifest_specs_tests {
    use serde_json::Value;

    // Regression guard: an action that reaches the model with an empty
    // description gets dropped by the agent's embedding filter. This reads
    // the shipped plugin.json directly (not action_specs(), which relies on
    // current_exe() and is meaningless in a dev-build test binary).
    #[test]
    fn shipped_manifest_documents_every_declared_action() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let declared = parsed["actions"].as_array().unwrap().len();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        assert_eq!(
            specs.len(),
            declared,
            "every declared action must produce a spec"
        );
        let undocumented: Vec<&str> = specs
            .iter()
            .filter(|s| s.description.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            undocumented.is_empty(),
            "actions missing a description: {undocumented:?}"
        );
    }
}
