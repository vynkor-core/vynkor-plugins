//! Secrets-first provider API-key resolution. The `api_key_env` handle a
//! caller passes names the key in BOTH places: first the `secrets` plugin's
//! vault (via its `secret_get` action, gated by `PERMISSION_SECRETS`), then
//! the plugin's own process environment. Vault wins when both exist, and the
//! key is resolved per request (no cache) so vault rotation takes effect
//! immediately. The resolved key value never appears in any error or log line.

use vynkor_sdk::proto::ActionStatus;

use crate::outbound::ActionCaller;

/// `secrets`' `secret_get` response shape — see the secrets plugin.
#[derive(serde::Deserialize)]
struct SecretGetResponse {
    found: bool,
    #[serde(default)]
    value: String,
}

/// How long to wait for the `secrets` plugin to answer before falling back.
const SECRETS_TIMEOUT_MS: u32 = 3000;

/// Resolve the provider API key for `handle`: vault first, env var second.
/// Returns `Err` only when neither source has a non-empty value.
pub async fn resolve_api_key(
    caller: &mut impl ActionCaller,
    handle: &str,
) -> Result<String, String> {
    // Hop 1: the `secrets` plugin's vault. Any failure here — network-level
    // error, non-OK status, malformed reply, not found, or an empty value —
    // is logged (without the value) and falls through to the env fallback.
    let params = serde_json::json!({"name": handle});
    match caller
        .call_action(
            "secret_get",
            &params.to_string().into_bytes(),
            SECRETS_TIMEOUT_MS,
        )
        .await
    {
        Ok(resp) if resp.status == ActionStatus::ActionOk as i32 => {
            match serde_json::from_slice::<SecretGetResponse>(&resp.data_json) {
                Ok(secret) if secret.found && !secret.value.is_empty() => {
                    return Ok(secret.value);
                }
                Ok(_) => {
                    eprintln!(
                        "[ai] secrets vault has no non-empty value for '{handle}', \
                         falling back to env var '{handle}'"
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[ai] secrets vault returned a malformed reply for '{handle}' \
                         ({e}), falling back to env var '{handle}'"
                    );
                }
            }
        }
        Ok(resp) => {
            eprintln!(
                "[ai] secrets vault unavailable for '{handle}' ({}), \
                 falling back to env var '{handle}'",
                resp.error
            );
        }
        Err(e) => {
            eprintln!(
                "[ai] secrets vault unavailable for '{handle}' ({e}), \
                 falling back to env var '{handle}'"
            );
        }
    }

    // Hop 2: the plugin's own environment (the pre-vault mechanism).
    if let Ok(value) = std::env::var(handle) {
        if !value.is_empty() {
            return Ok(value);
        }
    }

    Err(format!(
        "key '{handle}' is neither in the secrets vault nor set as an environment variable"
    ))
}
