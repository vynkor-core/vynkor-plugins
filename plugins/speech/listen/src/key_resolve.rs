//! Provider API-key resolution for the cloud (`openai`) provider.
//!
//! Secrets-first: the key is read from the `stt` plugin's own `secrets`
//! vault via the `secret_get` action, with the existing environment
//! variable mechanism as fallback. The env-var-style handle the caller
//! passes (`api_key_env`) is the lookup name for *both* sources; the
//! vault wins when both exist.
//!
//! The resolved key value is never logged and never embedded in an error
//! string — only the handle (name) appears in diagnostics.

use vynkor_sdk::VynkorClient;

/// `secret_get`'s response shape (see `plugins/secrets/src/handler.rs`):
/// `{"found":true,"value":"..."}` or `{"found":false}`.
#[derive(serde::Deserialize)]
struct SecretGetResponse {
    found: bool,
    #[serde(default)]
    value: String,
}

/// Timeout for the `secret_get` hop to the `secrets` plugin.
const SECRETS_TIMEOUT_MS: u32 = 3000;

/// Resolve the API key for the cloud provider.
///
/// Resolution order:
/// 1. `secrets` vault: `secret_get {"name": handle}` — returned when the
///    action is OK, the secret is `found`, and its value is non-empty.
/// 2. Environment variable `handle` — fallback, returned when non-empty.
///
/// Any failure of the vault hop (non-OK status, malformed reply, not
/// found, empty value, `send_action` error) logs to stderr with the
/// `[stt]` prefix and falls through to the env var. `Err` only when both
/// sources miss.
pub async fn resolve_api_key(client: &mut VynkorClient, handle: &str) -> Result<String, String> {
    let params = serde_json::json!({ "name": handle })
        .to_string()
        .into_bytes();
    match client
        .send_action("secret_get", &params, SECRETS_TIMEOUT_MS)
        .await
    {
        Ok(resp) => {
            if resp.status == vynkor_sdk::proto::ActionStatus::ActionOk as i32 {
                match serde_json::from_slice::<SecretGetResponse>(&resp.data_json) {
                    Ok(secret) if secret.found && !secret.value.is_empty() => {
                        return Ok(secret.value);
                    }
                    Ok(_) => eprintln!(
                        "[stt] secrets vault has no '{handle}' entry, falling back to env var '{handle}'"
                    ),
                    Err(e) => eprintln!(
                        "[stt] secrets vault returned a malformed reply ({e}), falling back to env var '{handle}'"
                    ),
                }
            } else {
                eprintln!(
                    "[stt] secrets vault unavailable ({}), falling back to env var '{handle}'",
                    resp.error
                );
            }
        }
        Err(e) => {
            eprintln!("[stt] secrets vault unavailable ({e}), falling back to env var '{handle}'")
        }
    }

    match std::env::var(handle) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(format!(
            "key '{handle}' is neither in the secrets vault nor set as an environment variable"
        )),
    }
}
