//! Tool names on the wire. OpenAI-compatible providers accept only
//! `^[a-zA-Z0-9_-]{1,64}$` and reject the whole request otherwise, while
//! kernel tool names may carry a device prefix (`pixel-6pro.calendar`). The
//! request gets a safe, unique alias per tool; tool calls in the reply are
//! mapped back to the caller's names.

use crate::provider::ToolCall;
use crate::request::ToolSpec;

const MAX_LEN: usize = 64;

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_LEN)
        .collect();
    if s.is_empty() {
        "tool".into()
    } else {
        s
    }
}

/// The wire name of each tool, in order: the name itself when it is valid,
/// otherwise a sanitized alias made unique among the request's tools.
pub fn wire_names(tools: &[ToolSpec]) -> Vec<String> {
    let mut taken: std::collections::HashSet<String> = tools
        .iter()
        .filter(|t| sanitize(&t.name) == t.name)
        .map(|t| t.name.clone())
        .collect();
    tools
        .iter()
        .map(|t| {
            let base = sanitize(&t.name);
            if base == t.name {
                return base;
            }
            let mut alias = base.clone();
            let mut n = 2;
            while !taken.insert(alias.clone()) {
                let suffix = format!("_{n}");
                let keep = MAX_LEN.saturating_sub(suffix.len()).min(base.len());
                alias = format!("{}{suffix}", &base[..keep]);
                n += 1;
            }
            alias
        })
        .collect()
}

/// Map tool calls named by wire alias back to the caller's tool names.
pub fn restore(calls: &mut [ToolCall], tools: &[ToolSpec]) {
    if tools.is_empty() || calls.is_empty() {
        return;
    }
    let wire = wire_names(tools);
    for call in calls {
        if let Some(i) = wire.iter().position(|w| *w == call.name) {
            call.name = tools[i].name.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments_json: "{}".into(),
        }
    }

    #[test]
    fn valid_names_pass_through_and_dotted_ones_get_an_alias() {
        let tools = [tool("weather_now"), tool("pixel-6pro.calendar")];
        assert_eq!(wire_names(&tools), ["weather_now", "pixel-6pro_calendar"]);
    }

    #[test]
    fn an_alias_never_collides_with_a_real_name() {
        let tools = [tool("a.b"), tool("a_b"), tool("a:b")];
        assert_eq!(wire_names(&tools), ["a_b_2", "a_b", "a_b_3"]);
    }

    #[test]
    fn long_names_are_cut_to_the_provider_limit() {
        let tools = [tool(&format!("dev.{}", "x".repeat(100)))];
        let w = wire_names(&tools);
        assert_eq!(w[0].len(), MAX_LEN);
        assert!(w[0].starts_with("dev_x"));
    }

    #[test]
    fn calls_come_back_under_the_callers_names() {
        let tools = [tool("weather_now"), tool("pixel-6pro.calendar")];
        let mut calls = [
            call("pixel-6pro_calendar"),
            call("weather_now"),
            call("other"),
        ];
        restore(&mut calls, &tools);
        assert_eq!(calls[0].name, "pixel-6pro.calendar");
        assert_eq!(calls[1].name, "weather_now");
        assert_eq!(calls[2].name, "other");
    }
}
