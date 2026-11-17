//! Action param validation for the `hotkey` plugin. Pure functions so
//! every malformed-request path is unit-testable without a kernel.

/// One parsed action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyRequest {
    Bind {
        id: String,
        trigger: String,
        description: String,
    },
    Unbind {
        id: String,
    },
    List,
    Inject {
        binding: String,
        pressed: bool,
    },
}

const MAX_DESCRIPTION_CHARS: usize = 200;
const MAX_TRIGGER_CHARS: usize = 64;

/// Parse one action's params. Errors name the offending field — they land
/// verbatim in `ActionResponse.error`.
pub fn parse(action: &str, params_json: &[u8]) -> Result<HotkeyRequest, String> {
    let params: serde_json::Value =
        serde_json::from_slice(params_json).map_err(|e| format!("malformed params_json: {e}"))?;

    match action {
        "hotkey_bind" => {
            let id = string_field(&params, "id")?;
            crate::bindings::validate_id(&id)?;
            let trigger = string_field(&params, "trigger")?;
            if trigger.len() > MAX_TRIGGER_CHARS {
                return Err(format!("trigger exceeds {MAX_TRIGGER_CHARS} chars"));
            }
            let normalized = crate::bindings::normalize_trigger(&trigger)
                .map_err(|e| format!("invalid trigger: {e}"))?;
            let description = match params.get("description") {
                None | Some(serde_json::Value::Null) => format!("hotkey {id}"),
                Some(serde_json::Value::String(s)) => {
                    let s = s.trim();
                    if s.len() > MAX_DESCRIPTION_CHARS {
                        return Err(format!("description exceeds {MAX_DESCRIPTION_CHARS} chars"));
                    }
                    s.to_string()
                }
                Some(other) => return Err(format!("description must be a string, got {other}")),
            };
            Ok(HotkeyRequest::Bind {
                id,
                trigger: normalized,
                description,
            })
        }
        "hotkey_unbind" => {
            let id = string_field(&params, "id")?;
            Ok(HotkeyRequest::Unbind { id })
        }
        "hotkey_list" => Ok(HotkeyRequest::List),
        "hotkey_status" => Ok(HotkeyRequest::List),
        "hotkey_inject" => {
            let binding = string_field(&params, "binding")?;
            let state = string_field(&params, "state")?;
            let pressed = match state.as_str() {
                "pressed" => true,
                "released" => false,
                other => {
                    return Err(format!(
                        "state must be 'pressed' or 'released', got '{other}'"
                    ))
                }
            };
            Ok(HotkeyRequest::Inject { binding, pressed })
        }
        other => Err(format!("unknown action: {other}")),
    }
}

fn string_field(params: &serde_json::Value, field: &str) -> Result<String, String> {
    match params.get(field) {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        Some(serde_json::Value::String(_)) => Err(format!("{field} must not be empty")),
        Some(other) => Err(format!("{field} must be a string, got {other}")),
        None => Err(format!("missing required field '{field}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn err_for(action: &str, params: serde_json::Value) -> String {
        parse(action, params.to_string().as_bytes()).unwrap_err()
    }

    #[test]
    fn bind_parses_and_normalizes() {
        let req = parse(
            "hotkey_bind",
            json!({"id": "ptt", "trigger": "Ctrl+Shift+Space", "description": "talk"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            req,
            HotkeyRequest::Bind {
                id: "ptt".into(),
                trigger: "CTRL+SHIFT+space".into(),
                description: "talk".into(),
            }
        );
    }

    #[test]
    fn bind_defaults_description_to_the_id() {
        let req = parse(
            "hotkey_bind",
            json!({"id": "mute", "trigger": "Super+M"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        match req {
            HotkeyRequest::Bind { description, .. } => assert_eq!(description, "hotkey mute"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bind_field_errors_name_the_field() {
        assert!(err_for("hotkey_bind", json!({})).contains("'id'"));
        assert!(err_for("hotkey_bind", json!({"id": "ptt"})).contains("'trigger'"));
        assert!(
            err_for("hotkey_bind", json!({"id": "BAD ID", "trigger": "Ctrl+a"}))
                .contains("invalid binding id")
        );
        assert!(
            err_for("hotkey_bind", json!({"id": "ptt", "trigger": "Space"}))
                .contains("invalid trigger")
        );
        assert!(err_for(
            "hotkey_bind",
            json!({"id": "ptt", "trigger": "Ctrl+a", "description": 5})
        )
        .contains("description must be a string"));
    }

    #[test]
    fn inject_requires_a_known_state() {
        let req = parse(
            "hotkey_inject",
            json!({"binding": "ptt", "state": "pressed"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            req,
            HotkeyRequest::Inject {
                binding: "ptt".into(),
                pressed: true
            }
        );

        let req = parse(
            "hotkey_inject",
            json!({"binding": "ptt", "state": "released"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            req,
            HotkeyRequest::Inject {
                binding: "ptt".into(),
                pressed: false
            }
        );

        assert!(
            err_for("hotkey_inject", json!({"binding": "p", "state": "held"}))
                .contains("'pressed' or 'released'")
        );
        assert!(err_for("hotkey_inject", json!({"state": "pressed"})).contains("'binding'"));
    }

    #[test]
    fn unbind_list_status_shapes() {
        assert_eq!(
            parse("hotkey_unbind", json!({"id": "ptt"}).to_string().as_bytes()).unwrap(),
            HotkeyRequest::Unbind { id: "ptt".into() }
        );
        assert_eq!(parse("hotkey_list", b"{}").unwrap(), HotkeyRequest::List);
        assert!(parse("nope", b"{}").unwrap_err().contains("unknown action"));
    }
}
