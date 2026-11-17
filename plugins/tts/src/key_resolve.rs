//! Provider API-key resolution for the cloud providers (`openai`,
//! `elevenlabs`): secrets-first, with the process environment as fallback.
//!
//! The caller passes an env-var-style *handle* (`api_key_env`), which is
//! used as the lookup name in BOTH places:
//!
//!   1. the `secrets` plugin's vault — `secret_get {"name": handle}`;
//!   2. the `tts` process environment — `std::env::var(handle)`.
//!
//! The vault wins when both hold a value. The resolved key value is never
//! logged and never embedded in an error string — only the handle name is.

use vynkor_sdk::VynkorClient;

/// Reply shape of `secrets`'s `secret_get` action
/// (`{"found":true,"value":"..."}` / `{"found":false}`).
#[derive(serde::Deserialize)]
struct SecretGetResponse {
    found: bool,
    #[serde(default)]
    value: String,
}

/// Timeout for the `secret_get` hop to the `secrets` plugin. A vault miss
/// is not fatal — the env fallback is tried next.
const SECRETS_TIMEOUT_MS: u32 = 3000;

/// Resolve the API key the caller referenced by `handle`.
///
/// Order: (1) the `secrets` vault under `handle`; (2) the environment
/// variable named `handle`. Any failure of the vault hop — non-OK status,
/// malformed reply, not found, empty value, or a `send_action` error — is
/// logged to stderr (value never included) and the env fallback is tried.
/// `Err` only when both sources miss.
pub async fn resolve_api_key(client: &mut VynkorClient, handle: &str) -> Result<String, String> {
    match client
        .send_action(
            "secret_get",
            &serde_json::json!({ "name": handle })
                .to_string()
                .into_bytes(),
            SECRETS_TIMEOUT_MS,
        )
        .await
    {
        Ok(resp) => {
            if resp.status == vynkor_sdk::proto::ActionStatus::ActionOk as i32 {
                match serde_json::from_slice::<SecretGetResponse>(&resp.data_json) {
                    Ok(secret) if secret.found && !secret.value.is_empty() => {
                        return Ok(secret.value);
                    }
                    Ok(_) => eprintln!(
                        "[tts] secrets vault unavailable (not found or empty), \
                         falling back to env var '{handle}'"
                    ),
                    Err(e) => eprintln!(
                        "[tts] secrets vault unavailable (malformed reply: {e}), \
                         falling back to env var '{handle}'"
                    ),
                }
            } else {
                eprintln!(
                    "[tts] secrets vault unavailable (error: {}), \
                     falling back to env var '{handle}'",
                    resp.error
                );
            }
        }
        Err(e) => {
            eprintln!(
                "[tts] secrets vault unavailable ({e}), \
                 falling back to env var '{handle}'"
            );
        }
    }

    match std::env::var(handle) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(format!(
            "key '{handle}' is neither in the secrets vault nor set as an environment variable"
        )),
    }
}
