//! Request parsing and validation for the `daemon` plugin's actions.
//!
//! serde enforces nothing beyond types (a manifest `"minLength"` is
//! documentation, not a check), so every field is validated loudly here with
//! errors that name the offending field — the shipped-plugin convention.

use serde::Deserialize;
use serde_json::Value;

/// Text caps shared by every prompt-shaped field. 4000 matches tts's own
/// synthesis limit — anything longer fails there anyway, so reject earlier
/// with a clearer message.
pub const MAX_TEXT_CHARS: usize = 4000;

#[derive(Debug)]
pub enum DaemonRequest {
    Enable,
    Disable,
    Status,
    /// One voice cycle. `text` bypasses the mic/stt listen stage.
    Turn {
        text: Option<String>,
    },
    /// Synthesize + play text through `tts` then `sound`.
    Say {
        text: String,
    },
    /// Agent round-trip; the answer is spoken aloud.
    Ask {
        prompt: String,
    },
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<DaemonRequest, String> {
    let params: Value = if params_json.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(params_json).map_err(|e| {
            format!("ERR_DAEMON_BAD_PARAMS: malformed params_json for {action}: {e}")
        })?
    };

    match action {
        "daemon_enable" => {
            expect_empty(&params)?;
            Ok(DaemonRequest::Enable)
        }
        "daemon_disable" => {
            expect_empty(&params)?;
            Ok(DaemonRequest::Disable)
        }
        "daemon_status" => {
            expect_empty(&params)?;
            Ok(DaemonRequest::Status)
        }
        "daemon_turn" => {
            #[derive(Deserialize)]
            struct TurnParams {
                #[serde(default)]
                text: Option<String>,
            }
            let p: TurnParams = parse_params(&params)?;
            let text = match p.text {
                Some(t) => Some(validate_text("text", &t)?),
                None => None,
            };
            Ok(DaemonRequest::Turn { text })
        }
        "daemon_say" => {
            #[derive(Deserialize)]
            struct SayParams {
                text: Option<String>,
            }
            let p: SayParams = parse_params(&params)?;
            let text = p
                .text
                .ok_or_else(|| "ERR_DAEMON_BAD_PARAMS: missing required field: text".to_string())?;
            Ok(DaemonRequest::Say {
                text: validate_text("text", &text)?,
            })
        }
        "daemon_ask" => {
            #[derive(Deserialize)]
            struct AskParams {
                prompt: Option<String>,
            }
            let p: AskParams = parse_params(&params)?;
            let prompt = p.prompt.ok_or_else(|| {
                "ERR_DAEMON_BAD_PARAMS: missing required field: prompt".to_string()
            })?;
            Ok(DaemonRequest::Ask {
                prompt: validate_text("prompt", &prompt)?,
            })
        }
        other => Err(format!("unknown action: {other}")),
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T, String> {
    if params.is_null() {
        // An empty body means `{}` for actions whose params are all optional.
        return serde_json::from_value(Value::Object(Default::default()))
            .map_err(|e| format!("ERR_DAEMON_BAD_PARAMS: {e}"));
    }
    serde_json::from_value(params.clone())
        .map_err(|e| format!("ERR_DAEMON_BAD_PARAMS: malformed params: {e}"))
}

fn expect_empty(params: &Value) -> Result<(), String> {
    match params {
        Value::Null => Ok(()),
        Value::Object(o) if o.is_empty() => Ok(()),
        other => Err(format!(
            "ERR_DAEMON_BAD_PARAMS: expected empty params, got {other}"
        )),
    }
}

fn validate_text(field: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("ERR_DAEMON_BAD_PARAMS: non-empty {field} required"));
    }
    if trimmed.chars().count() > MAX_TEXT_CHARS {
        return Err(format!(
            "ERR_DAEMON_BAD_PARAMS: {field} exceeds {MAX_TEXT_CHARS} chars"
        ));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(action: &str, params: Value) -> Result<DaemonRequest, String> {
        parse_request(action, &serde_json::to_vec(&params).unwrap())
    }

    #[test]
    fn enable_disable_status_accept_empty_params() {
        for action in ["daemon_enable", "daemon_disable", "daemon_status"] {
            assert!(parse(action, serde_json::json!({})).is_ok(), "{action}");
            assert!(parse(action, Value::Null).is_ok(), "{action} null body");
        }
    }

    #[test]
    fn enable_rejects_extra_fields() {
        let err = parse("daemon_enable", serde_json::json!({"force": true})).unwrap_err();
        assert!(err.contains("empty params"), "error was: {err}");
    }

    #[test]
    fn turn_without_text_is_listen_turn() {
        match parse("daemon_turn", serde_json::json!({})).unwrap() {
            DaemonRequest::Turn { text } => assert_eq!(text, None),
            other => panic!("expected Turn, got {other:?}"),
        }
        // A JSON body is optional entirely.
        match parse_request("daemon_turn", b"").unwrap() {
            DaemonRequest::Turn { text } => assert_eq!(text, None),
            other => panic!("expected Turn, got {other:?}"),
        }
    }

    #[test]
    fn turn_with_text_bypasses_listen() {
        match parse("daemon_turn", serde_json::json!({"text": "  hi  "})).unwrap() {
            DaemonRequest::Turn { text } => assert_eq!(text.as_deref(), Some("hi")),
            other => panic!("expected Turn, got {other:?}"),
        }
    }

    #[test]
    fn turn_text_validated() {
        let err = parse("daemon_turn", serde_json::json!({"text": "   "})).unwrap_err();
        assert!(err.contains("non-empty text"), "error was: {err}");

        let long = "x".repeat(MAX_TEXT_CHARS + 1);
        let err = parse("daemon_turn", serde_json::json!({ "text": long })).unwrap_err();
        assert!(err.contains("exceeds"), "error was: {err}");
    }

    #[test]
    fn say_requires_nonempty_text() {
        let err = parse("daemon_say", serde_json::json!({})).unwrap_err();
        assert!(
            err.contains("missing required field: text"),
            "error was: {err}"
        );

        let err = parse("daemon_say", serde_json::json!({"text": ""})).unwrap_err();
        assert!(err.contains("non-empty text"), "error was: {err}");

        match parse("daemon_say", serde_json::json!({"text": "hello"})).unwrap() {
            DaemonRequest::Say { text } => assert_eq!(text, "hello"),
            other => panic!("expected Say, got {other:?}"),
        }
    }

    #[test]
    fn ask_requires_prompt_and_trims() {
        let err = parse("daemon_ask", serde_json::json!({})).unwrap_err();
        assert!(
            err.contains("missing required field: prompt"),
            "error was: {err}"
        );

        match parse("daemon_ask", serde_json::json!({"prompt": " what? "})).unwrap() {
            DaemonRequest::Ask { prompt } => assert_eq!(prompt, "what?"),
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn unknown_action_names_itself() {
        let err = parse("daemon_frobnicate", serde_json::json!({})).unwrap_err();
        assert!(err.contains("unknown action"), "error was: {err}");
    }

    #[test]
    fn malformed_json_names_the_action() {
        let err = parse_request("daemon_turn", b"{not json").unwrap_err();
        assert!(err.contains("daemon_turn"), "error was: {err}");
    }
}
