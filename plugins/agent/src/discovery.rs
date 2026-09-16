//! Runtime capability discovery: build tool specs from the registered
//! plugins' own manifests via the kernel's read-only `list_plugins` +
//! `get_manifest` commands (exempt from `PERMISSION_KERNEL_ADMIN` — see the
//! kernel's `READONLY_COMMANDS`). This is what makes the agent "pull its
//! knowledge from the plugins" instead of relying on a hand-written file.
//!
//! Action names are globally unique across plugins (the kernel refuses
//! ambiguous manifest declarations), so bare action names work as tool
//! names. The agent's own actions are skipped — they are the loop's API,
//! not tools for the model.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::tools::{Source, ToolSpec, TOOL_TIMEOUT_DEFAULT_MS};
use crate::Rpc;

/// Per-command timeout for discovery round-trips (ms).
const DISCOVERY_TIMEOUT_MS: u32 = 5_000;

/// `ActionSpecV2` (`vynkor-wire::manifest::ActionSpecV2`) carries no
/// tool-level `description` field — only `name`/`permission`/`input`/
/// `output` (the legacy v1 `ActionSpec` had one; V2 dropped it). Without a
/// fallback here every V2-declared tool embeds as `"name — "`, which the
/// embedding-similarity filter (`agent::engine::embedding_filtered_catalog`)
/// then reliably drops for any goal text that doesn't literally contain the
/// tool name. Until `vynkor-wire` grows a real field, stitch a description
/// together from the per-parameter docs plugin authors already write.
fn description_from_parameters(parameters: &Value) -> String {
    parameters
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .values()
                .filter_map(|p| p.get("description").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default()
}

fn spec_from_action_spec(s: &Value) -> Option<ToolSpec> {
    let name = s.get("name").and_then(Value::as_str)?;
    let parameters = match s.get("params_schema") {
        // The kernel carries params_schema as a JSON-encoded string.
        Some(Value::String(raw)) => {
            serde_json::from_str::<Value>(raw).ok().filter(Value::is_object).unwrap_or(Value::Null)
        }
        Some(p @ Value::Object(_)) => p.clone(),
        _ => Value::Null,
    };
    let mut description =
        s.get("description").and_then(Value::as_str).unwrap_or_default().to_string();
    if description.is_empty() {
        description = description_from_parameters(&parameters);
    }
    Some(ToolSpec {
        name: name.to_string(),
        description,
        parameters,
        requires_confirmation: s
            .get("requires_confirmation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        risk: s.get("risk").and_then(Value::as_str).unwrap_or_default().to_string(),
        timeout_ms: TOOL_TIMEOUT_DEFAULT_MS,
        cooldown_ms: 0,
        max_per_goal: 16,
        source: Source::Kernel,
    })
}

async fn plugin_manifest(rpc: &Rpc, slug: &str) -> Result<Value, String> {
    rpc.call_command(
        "get_manifest",
        json!({ "plugin_id": slug }),
        DISCOVERY_TIMEOUT_MS,
    )
    .await
}

/// Enumerate every registered plugin and collect its `action_specs` into
/// tool specs keyed by action name.
pub async fn discover(rpc: &Rpc) -> Result<BTreeMap<String, ToolSpec>, String> {
    let list = rpc.call_command("list_plugins", json!({}), DISCOVERY_TIMEOUT_MS).await?;
    let entries = list
        .as_array()
        .ok_or_else(|| format!("list_plugins returned unexpected payload: {list}"))?;

    let mut out = BTreeMap::new();
    for entry in entries {
        let slug = entry.get("plugin_id").and_then(Value::as_str).unwrap_or_default();
        if slug.is_empty() || slug == "agent" {
            continue;
        }
        let manifest = plugin_manifest(rpc, slug).await?;
        let specs = manifest.get("action_specs").and_then(Value::as_array);
        for s in specs.into_iter().flatten() {
            if let Some(spec) = spec_from_action_spec(s) {
                out.insert(spec.name.clone(), spec);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kernel_action_spec_shape() {
        let s = json!({
            "name": "fs_read",
            "description": "Read a file",
            "params_schema": "{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}}}",
            "risk": "medium",
            "requires_confirmation": true
        });
        let t = spec_from_action_spec(&s).unwrap();
        assert_eq!(t.name, "fs_read");
        assert_eq!(t.source, Source::Kernel);
        assert!(t.requires_confirmation);
        assert_eq!(t.risk, "medium");
        assert_eq!(t.parameters["properties"]["path"]["type"], "string");
    }

    #[test]
    fn malformed_schema_degrades_to_null_not_error() {
        let s = json!({"name": "x", "params_schema": "not json", "description": ""});
        let t = spec_from_action_spec(&s).unwrap();
        assert_eq!(t.parameters, Value::Null);

        let missing = json!({"name": "y"});
        let t = spec_from_action_spec(&missing).unwrap();
        assert_eq!(t.parameters, Value::Null);
        assert_eq!(t.risk, "");
    }

    #[test]
    fn missing_top_level_description_falls_back_to_param_docs() {
        // ActionSpecV2 (vynkor-wire::manifest) has no description field —
        // this is the shape a V2-declared tool (e.g. daemon_say) actually
        // arrives in.
        let s = json!({
            "name": "daemon_say",
            "input": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Text to synthesize and play through tts + sound."}
                }
            },
            "params_schema": "{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\",\"description\":\"Text to synthesize and play through tts + sound.\"}}}"
        });
        let t = spec_from_action_spec(&s).unwrap();
        assert_eq!(t.description, "Text to synthesize and play through tts + sound.");
    }

    #[test]
    fn entry_without_name_is_skipped() {
        assert!(spec_from_action_spec(&json!({"description": "?"})).is_none());
    }
}
