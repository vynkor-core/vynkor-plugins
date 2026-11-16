//! Broker password for the startup auto-connect: the `secrets` vault first
//! (`secret_get`, under [`PASSWORD_NAME`]), then the env var of the same name.
//!
//! Put it in the vault with
//! `printf %s "$PW" | secrets-provision mqtt MQTT_PLUGIN_PASSWORD` and drop
//! it from `plugins.d/mqtt.yaml`. The value is never logged.

use std::time::Duration;

use vynkor_sdk::proto::ActionStatus;
use vynkor_sdk::VynkorClient;

pub const PASSWORD_NAME: &str = "MQTT_PLUGIN_PASSWORD";

const SECRETS_TIMEOUT_MS: u32 = 3000;
/// `secrets` may register after us at kernel start: retry an unreachable
/// vault this many times, a second apart. A clean "not found" is final.
const VAULT_ATTEMPTS: u32 = 5;
const VAULT_RETRY: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum VaultAnswer {
    Found(String),
    Missing,
    Unavailable(String),
}

#[derive(serde::Deserialize)]
struct SecretGetResponse {
    found: bool,
    #[serde(default)]
    value: String,
}

/// Where the password lookup asks first; the kernel's `secrets` plugin in
/// production, a script in tests.
pub(crate) trait Vault {
    async fn ask(&mut self, name: &str) -> VaultAnswer;
}

impl Vault for VynkorClient {
    async fn ask(&mut self, name: &str) -> VaultAnswer {
        ask_vault(self, name).await
    }
}

async fn ask_vault(client: &mut VynkorClient, name: &str) -> VaultAnswer {
    let params = serde_json::json!({ "name": name }).to_string();
    match client
        .send_action("secret_get", params.as_bytes(), SECRETS_TIMEOUT_MS)
        .await
    {
        Ok(resp) if resp.status == ActionStatus::ActionOk as i32 => {
            match serde_json::from_slice::<SecretGetResponse>(&resp.data_json) {
                Ok(s) if s.found && !s.value.is_empty() => VaultAnswer::Found(s.value),
                Ok(_) => VaultAnswer::Missing,
                Err(e) => VaultAnswer::Unavailable(format!("malformed reply: {e}")),
            }
        }
        Ok(resp) => VaultAnswer::Unavailable(resp.error),
        Err(e) => VaultAnswer::Unavailable(e.to_string()),
    }
}

/// The broker password and where it came from (`"vault"` / `"env"`).
pub async fn resolve_password(client: &mut VynkorClient) -> Option<(String, &'static str)> {
    let env = std::env::var(PASSWORD_NAME).ok();
    resolve_with(client, env, VAULT_RETRY).await
}

async fn resolve_with(
    vault: &mut impl Vault,
    env: Option<String>,
    retry: Duration,
) -> Option<(String, &'static str)> {
    for attempt in 1..=VAULT_ATTEMPTS {
        match vault.ask(PASSWORD_NAME).await {
            VaultAnswer::Found(value) => return Some((value, "vault")),
            VaultAnswer::Missing => break,
            VaultAnswer::Unavailable(error) if attempt == VAULT_ATTEMPTS => {
                tracing::warn!(%error, "secrets vault unreachable; trying env {PASSWORD_NAME}");
            }
            VaultAnswer::Unavailable(_) => tokio::time::sleep(retry).await,
        }
    }
    env.filter(|value| !value.is_empty())
        .map(|value| (value, "env"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hands out scripted answers, then reports the vault as down.
    struct Script {
        answers: std::vec::IntoIter<VaultAnswer>,
        calls: u32,
    }

    impl Vault for Script {
        async fn ask(&mut self, name: &str) -> VaultAnswer {
            assert_eq!(name, PASSWORD_NAME);
            self.calls += 1;
            self.answers
                .next()
                .unwrap_or(VaultAnswer::Unavailable("down".into()))
        }
    }

    async fn run(
        answers: Vec<VaultAnswer>,
        env: Option<&str>,
    ) -> (Option<(String, &'static str)>, u32) {
        let mut script = Script {
            answers: answers.into_iter(),
            calls: 0,
        };
        let out = resolve_with(&mut script, env.map(String::from), Duration::ZERO).await;
        (out, script.calls)
    }

    #[tokio::test]
    async fn vault_wins_over_env() {
        let (out, _) = run(vec![VaultAnswer::Found("v".into())], Some("e")).await;
        assert_eq!(out, Some(("v".into(), "vault")));
    }

    #[tokio::test]
    async fn missing_in_vault_falls_back_to_env_without_retrying() {
        let (out, calls) = run(vec![VaultAnswer::Missing], Some("e")).await;
        assert_eq!(out, Some(("e".into(), "env")));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn late_secrets_plugin_is_waited_for() {
        let answers = vec![
            VaultAnswer::Unavailable("no route".into()),
            VaultAnswer::Unavailable("no route".into()),
            VaultAnswer::Found("v".into()),
        ];
        let (out, calls) = run(answers, None).await;
        assert_eq!(out, Some(("v".into(), "vault")));
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn unreachable_vault_gives_up_then_uses_env() {
        let (out, calls) = run(vec![], Some("e")).await;
        assert_eq!(out, Some(("e".into(), "env")));
        assert_eq!(calls, VAULT_ATTEMPTS);
    }

    #[tokio::test]
    async fn nothing_anywhere_means_no_password() {
        let (out, _) = run(vec![VaultAnswer::Missing], Some("")).await;
        assert_eq!(out, None);
    }
}
