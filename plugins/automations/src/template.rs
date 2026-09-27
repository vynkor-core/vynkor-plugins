//! Payload templating for rule actions: `{{/json/pointer}}` placeholders in
//! the string values of `action.params_json`, resolved against the event
//! payload that fired the rule.
//!
//! - A string that is exactly one placeholder (`"{{/latency_ms}}"`) becomes
//!   the pointed value with its JSON type (number stays a number); `null`
//!   when the pointer misses.
//! - Placeholders inside longer text (`"{{/url}} is down"`) are interpolated:
//!   strings verbatim, other values as compact JSON, a miss as "".
//!
//! Only string values are rewritten — object keys never are — and nothing is
//! evaluated: a placeholder is a JSON pointer lookup and nothing more.
//! Resolved values are event data, so they are as trusted as the event
//! source.

use serde_json::Value;

/// Returns `template` with every placeholder resolved against `payload`.
pub fn render(template: &Value, payload: &Value) -> Value {
    match template {
        Value::String(s) => render_str(s, payload),
        Value::Array(items) => Value::Array(items.iter().map(|v| render(v, payload)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), render(v, payload))).collect()),
        other => other.clone(),
    }
}

/// True when `template` holds at least one placeholder, so rules without
/// any skip the rewrite entirely.
pub fn has_placeholders(template: &Value) -> bool {
    match template {
        Value::String(s) => s.contains("{{/"),
        Value::Array(items) => items.iter().any(has_placeholders),
        Value::Object(map) => map.values().any(has_placeholders),
        _ => false,
    }
}

fn render_str(s: &str, payload: &Value) -> Value {
    if let Some(ptr) = s.strip_prefix("{{").and_then(|r| r.strip_suffix("}}")) {
        if ptr.starts_with('/') && !ptr.contains("}}") && !ptr.contains("{{") {
            return payload.pointer(ptr).cloned().unwrap_or(Value::Null);
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{/") {
        let Some(len) = rest[start..].find("}}") else { break };
        out.push_str(&rest[..start]);
        let ptr = &rest[start + 2..start + len];
        match payload.pointer(ptr) {
            Some(Value::String(v)) => out.push_str(v),
            Some(v) => out.push_str(&v.to_string()),
            None => {}
        }
        rest = &rest[start + len + 2..];
    }
    out.push_str(rest);
    Value::String(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> Value {
        json!({"url": "https://a.example", "status": 503, "down_for_ms": 1500, "tags": ["x"], "nested": {"k": "v"}})
    }

    #[test]
    fn whole_placeholder_keeps_the_json_type() {
        let t = json!({"code": "{{/status}}", "tags": "{{/tags}}", "missing": "{{/nope}}"});
        assert_eq!(render(&t, &payload()), json!({"code": 503, "tags": ["x"], "missing": null}));
    }

    #[test]
    fn inline_placeholders_interpolate() {
        let t = json!({"message": "{{/url}} is down (HTTP {{/status}}){{/nope}}, {{/nested/k}}"});
        assert_eq!(render(&t, &payload()), json!({"message": "https://a.example is down (HTTP 503), v"}));
    }

    #[test]
    fn nested_values_arrays_and_plain_text_are_handled() {
        let t = json!({"a": [{"b": "{{/url}}"}, 5, true], "plain": "no braces {{ here }}", "{{/url}}": "key untouched"});
        assert_eq!(
            render(&t, &payload()),
            json!({"a": [{"b": "https://a.example"}, 5, true], "plain": "no braces {{ here }}", "{{/url}}": "key untouched"})
        );
    }

    #[test]
    fn unterminated_placeholder_is_left_as_text() {
        assert_eq!(render(&json!("x {{/url"), &payload()), json!("x {{/url"));
    }

    #[test]
    fn detects_placeholders() {
        assert!(has_placeholders(&json!({"a": ["{{/x}}"]})));
        assert!(!has_placeholders(&json!({"a": "{{x}}", "{{/k}}": 1})));
    }
}
