//! Put a secret into another plugin's vault (see `secrets_plugin::provision`).
//!
//! ```text
//! printf %s "$PASSWORD" | secrets-provision mqtt MQTT_PLUGIN_PASSWORD
//! ```
//!
//! Reads the value from stdin (one trailing newline is dropped) so it never
//! lands in argv or shell history, and the same `SECRETS_PLUGIN_DATA_DIR` /
//! `SECRETS_PLUGIN_MASTER_KEY` env the plugin runs with. Restart the
//! `secrets` plugin afterwards.

use std::io::Read;
use std::process::ExitCode;

use secrets_plugin::provision::provision;
use secrets_plugin::vault::parse_master_key;
use zeroize::Zeroizing;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [plugin_id, name] = args.as_slice() else {
        eprintln!("usage: secrets-provision <plugin_id> <secret_name>  (value on stdin)");
        return ExitCode::from(2);
    };
    match run(plugin_id, name) {
        Ok(()) => {
            println!("stored {name} in {plugin_id}'s vault; restart the secrets plugin");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("secrets-provision: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(plugin_id: &str, name: &str) -> Result<(), String> {
    let data_dir = std::env::var("SECRETS_PLUGIN_DATA_DIR")
        .map_err(|_| "SECRETS_PLUGIN_DATA_DIR must be set".to_string())?;
    let key_raw = Zeroizing::new(
        std::env::var("SECRETS_PLUGIN_MASTER_KEY")
            .map_err(|_| "SECRETS_PLUGIN_MASTER_KEY must be set".to_string())?,
    );
    let key = Zeroizing::new(parse_master_key(&key_raw)?);

    let mut value = String::new();
    std::io::stdin()
        .read_to_string(&mut value)
        .map_err(|e| format!("failed to read the value from stdin: {e}"))?;
    let mut value = Zeroizing::new(value);
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    provision(
        std::path::Path::new(&data_dir),
        &key,
        plugin_id,
        name,
        value.to_string(),
    )
}
