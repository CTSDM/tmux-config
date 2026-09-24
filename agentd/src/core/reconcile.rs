//! E2, reconciliation: what the pane's options say against what the agent
//! process and its transcript say.

use serde_json::Value;

use super::Op;

/// How many transcript lines are looked at (`tail -n 80`).
pub const TRANSCRIPT_LINES: usize = 80;

/// Claude's transcript says the turn is over: among its last lines, the last
/// `user`, `assistant` or `system`/`turn_duration` entry is that system
/// entry, or a user message starting with "[Request interrupted by user".
/// As the jq of agent-reconcile: lines that are not JSON, null or false are
/// skipped; any other entry that is not an object makes it undecidable, and
/// undecidable is not over.
pub fn turn_over<'a>(lines: impl IntoIterator<Item = &'a str>) -> bool {
    let mut last = None;
    for line in lines {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let entry = match entry {
            Value::Null | Value::Bool(false) => continue,
            Value::Object(o) => o,
            _ => return false,
        };
        let kind = entry.get("type").and_then(Value::as_str);
        let turn_end = kind == Some("system")
            && entry.get("subtype").and_then(Value::as_str) == Some("turn_duration");
        if matches!(kind, Some("user" | "assistant")) || turn_end {
            last = Some(entry);
        }
    }
    let Some(last) = last else { return false };
    match last.get("type").and_then(Value::as_str) {
        Some("system") => true,
        Some("user") => {
            let text = match last.get("message").and_then(|m| m.get("content")) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .map(|p| match p.get("text") {
                        Some(Value::String(s)) => s.as_str(),
                        _ => "",
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => return false,
            };
            text.starts_with("[Request interrupted by user")
        }
        _ => false,
    }
}

/// E2: a busy Claude pane whose turn is over becomes idle, without sound or
/// notification.
pub fn idle(now: i64) -> Vec<Op> {
    vec![
        Op::Set("@agent_state", "idle".into()),
        Op::Set("@agent_since", now.to_string()),
        Op::Unset("@agent_needs"),
        Op::Unset("@agent_needs_id"),
        Op::Unset("@agent_tool"),
    ]
}

/// E2: states a reconcile checks against the transcript.
pub fn busy(state: &str) -> bool {
    matches!(state, "working" | "needs" | "compacting")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn over(lines: &[&str]) -> bool {
        turn_over(lines.iter().copied())
    }

    #[test]
    fn e2_turn_duration_ends_the_turn() {
        assert!(over(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant"}"#,
            r#"{"type":"system","subtype":"turn_duration"}"#,
            r#"{"type":"summary"}"#,
            "not json",
            "null",
        ]));
        assert!(!over(&[
            r#"{"type":"system","subtype":"turn_duration"}"#,
            r#"{"type":"user","message":{"content":"next"}}"#,
        ]));
        assert!(!over(&[r#"{"type":"system","subtype":"other"}"#]));
    }

    #[test]
    fn e2_interrupted_by_the_user() {
        assert!(over(&[
            r#"{"type":"user","message":{"content":"[Request interrupted by user for tool use]"}}"#
        ]));
        assert!(over(&[
            r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"},{"type":"x"}]}}"#
        ]));
        assert!(!over(&[
            r#"{"type":"user","message":{"content":[{"type":"tool_result"},{"text":"[Request interrupted by user"}]}}"#
        ]));
        assert!(!over(&[r#"{"type":"user","message":{}}"#]));
    }

    #[test]
    fn e2_nothing_to_go_by_is_not_over() {
        assert!(!over(&[]));
        assert!(!over(&[r#"{"type":"assistant"}"#]));
        // jq fails on an entry that is not an object: undecidable.
        assert!(!over(&[
            r#"{"type":"system","subtype":"turn_duration"}"#,
            "5"
        ]));
    }

    #[test]
    fn e2_idle_writes() {
        assert_eq!(
            idle(7),
            vec![
                Op::Set("@agent_state", "idle".into()),
                Op::Set("@agent_since", "7".into()),
                Op::Unset("@agent_needs"),
                Op::Unset("@agent_needs_id"),
                Op::Unset("@agent_tool"),
            ]
        );
        assert!(busy("needs") && busy("compacting") && !busy("done") && !busy(""));
    }
}
