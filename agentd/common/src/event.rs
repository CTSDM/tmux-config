//! Hook events as the contract reads them (I3), shared by the desktop's
//! daemon and a server's holder.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Claude,
    Codex,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
        }
    }
}

/// The fields of a hook event that the contract uses (I3), already converted
/// to strings, whitespace collapsed and cut. Missing means empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    #[serde(rename = "hook_event_name")]
    pub ev: String,
    #[serde(rename = "session_id")]
    pub sid: String,
    pub agent_id: String,
    pub agent_type: String,
    #[serde(rename = "tool_name")]
    pub tool: String,
    #[serde(rename = "tool_use_id")]
    pub tool_id: String,
    pub detail: String,
    #[serde(rename = "notification_type")]
    pub ntype: String,
    #[serde(rename = "last_assistant_message")]
    pub last: String,
    pub error: String,
    #[serde(rename = "permission_mode")]
    pub mode: String,
    pub source: String,
    pub model: String,
    #[serde(rename = "transcript_path")]
    pub transcript: String,
    /// Codex: `turn_id`.
    #[serde(rename = "turn_id")]
    pub turn: String,
    /// Codex: the call's fingerprint (X2), computed by the hook so no tool
    /// input leaves it (X8).
    pub fingerprint: String,
    /// Codex: `tool_response.accepted` is true (X4).
    pub accepted: bool,
}

/// I3: every field is cut to this many characters.
pub const MAX_CHARS: usize = 300;

/// I3's `gsub("\\s+"; " ") | .[0:300]`: whitespace runs (Unicode, as jq's
/// `\s`) collapsed to one space, then cut to 300 characters.
pub fn line(text: &str) -> String {
    let mut out = String::new();
    let mut chars = 0;
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if in_space {
                continue;
            }
            in_space = true;
            out.push(' ');
        } else {
            in_space = false;
            out.push(c);
        }
        chars += 1;
        if chars == MAX_CHARS {
            break;
        }
    }
    out
}

/// I3: the fields the contract uses, as jq's `. // "" | tostring`, with
/// whitespace runs collapsed and cut to 300 characters. `None` when the
/// payload is not an object (jq fails on it) or null.
pub fn event_from_json(json: &Value) -> Option<Event> {
    let obj = match json {
        Value::Object(o) => o,
        Value::Null => return Some(Event::default()),
        _ => return None,
    };
    let field = |key: &str| value_line(present(obj.get(key)));
    let detail = match present(obj.get("tool_input")) {
        None => String::new(),
        Some(Value::Object(input)) => value_line(
            [
                "command",
                "file_path",
                "path",
                "pattern",
                "url",
                "query",
                "description",
            ]
            .iter()
            .find_map(|k| present(input.get(*k))),
        ),
        Some(other) => value_line(Some(other)),
    };
    Some(Event {
        ev: field("hook_event_name"),
        sid: field("session_id"),
        agent_id: field("agent_id"),
        agent_type: field("agent_type"),
        tool: field("tool_name"),
        tool_id: field("tool_use_id"),
        detail,
        ntype: field("notification_type"),
        last: field("last_assistant_message"),
        error: field("error"),
        mode: field("permission_mode"),
        source: field("source"),
        model: field("model"),
        transcript: field("transcript_path"),
        turn: match obj.get("turn_id") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        },
        // X2, X8: the call's fingerprint goes to the daemon, its input doesn't.
        fingerprint: fingerprint(
            obj.get("tool_name")
                .unwrap_or(&Value::String(String::new())),
            obj.get("tool_input")
                .unwrap_or(&Value::Object(serde_json::Map::new())),
        ),
        accepted: accepted(obj.get("tool_response")),
    })
}

/// X4: `tool_response.accepted` is true; the response may be a JSON string.
fn accepted(response: Option<&Value>) -> bool {
    let parsed;
    let response = match response {
        Some(Value::String(s)) => {
            parsed = serde_json::from_str::<Value>(s).unwrap_or(Value::Null);
            &parsed
        }
        Some(v) => v,
        None => return false,
    };
    response.get("accepted") == Some(&Value::Bool(true))
}

/// jq's `//`: null, false and missing are absent; everything else counts,
/// "" and 0 included.
fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

/// `tostring | gsub("\\s+"; " ") | .[0:300]` for a present value, "" else.
fn value_line(v: Option<&Value>) -> String {
    match v {
        None => String::new(),
        Some(Value::String(s)) => line(s),
        Some(other) => line(&other.to_string()),
    }
}

/// X2: the fingerprint of a call, `sha256(json.dumps([tool, input]))` as
/// agent-codex computes it (sorted keys, no spaces, ASCII escapes), with the
/// input's `description` left out.
pub fn fingerprint(tool: &Value, input: &Value) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let input = match input {
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| *k != "description")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    };
    let mut json = String::new();
    python_json(&Value::Array(vec![tool.clone(), input]), &mut json);
    let mut hex = String::with_capacity(64);
    for b in Sha256::digest(json.as_bytes()) {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// `json.dumps(v, sort_keys=True, separators=(",", ":"))`.
fn python_json(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&python_number(&n.to_string())),
        Value::String(s) => python_string(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                python_json(x, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                python_string(k, out);
                out.push(':');
                python_json(&o[*k], out);
            }
            out.push('}');
        }
    }
}

/// Python writes exponents with a sign and two digits at least (`1e+100`).
fn python_number(n: &str) -> String {
    match n.split_once('e') {
        Some((mantissa, exp)) => {
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp.strip_prefix('+').unwrap_or(exp)),
            };
            format!("{mantissa}e{sign}{digits:0>2}")
        }
        None => n.to_string(),
    }
}

/// A JSON string with everything outside printable ASCII escaped, as Python's
/// `ensure_ascii` does (UTF-16 surrogate pairs above U+FFFF).
fn python_string(s: &str, out: &mut String) {
    use std::fmt::Write;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{u:04x}");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(v: Value) -> Event {
        event_from_json(&v).unwrap()
    }

    #[test]
    fn i3_fields() {
        let e = event(json!({
            "hook_event_name": "PreToolUse", "session_id": "s1", "tool_name": "Bash",
            "tool_use_id": "t1", "tool_input": {"command": "ls  -la\n\tx"},
            "permission_mode": "plan", "transcript_path": "/t.jsonl", "model": "opus",
            "agent_id": "a", "agent_type": "Explore", "notification_type": "n",
            "last_assistant_message": "hi", "error": "e", "source": "startup"
        }));
        assert_eq!(e.ev, "PreToolUse");
        assert_eq!(e.detail, "ls -la x");
        assert_eq!(
            (e.sid.as_str(), e.tool.as_str(), e.tool_id.as_str()),
            ("s1", "Bash", "t1")
        );
        assert_eq!(
            (e.mode.as_str(), e.transcript.as_str(), e.model.as_str()),
            ("plan", "/t.jsonl", "opus")
        );
        assert_eq!(
            (e.agent_id.as_str(), e.agent_type.as_str(), e.ntype.as_str()),
            ("a", "Explore", "n")
        );
        assert_eq!(
            (e.last.as_str(), e.error.as_str(), e.source.as_str()),
            ("hi", "e", "startup")
        );
    }

    #[test]
    fn i3_missing_null_false_are_empty() {
        let e = event(
            json!({"hook_event_name": null, "session_id": false, "tool_name": 0, "error": true}),
        );
        assert_eq!(e.ev, "");
        assert_eq!(e.sid, "");
        assert_eq!(e.tool, "0");
        assert_eq!(e.error, "true");
        assert_eq!(e.last, "");
    }

    #[test]
    fn i3_json_text_for_non_strings() {
        let e = event(json!({"last_assistant_message": {"z": 1, "a": [1, 2]}, "error": 12}));
        // jq keeps the keys in input order.
        assert_eq!(e.last, r#"{"z":1,"a":[1,2]}"#);
        assert_eq!(e.error, "12");
    }

    #[test]
    fn i3_whitespace_like_jq() {
        // jq's \s: Unicode White_Space (NBSP, em space, ideographic space,
        // NEL), not the zero width space.
        let e = event(
            json!({"last_assistant_message": "x\u{a0}\u{a0}y\u{2003}z\u{200b}w\u{3000}v\u{85}u\tt\n\ns"}),
        );
        assert_eq!(e.last, "x y z\u{200b}w v u t s");
        let e = event(json!({"last_assistant_message": "\n  lead"}));
        assert_eq!(e.last, " lead");
    }

    #[test]
    fn i3_cut_to_300_characters_after_collapsing() {
        let long = format!("{}{}", "é".repeat(300), "  xyz");
        let e = event(json!({"last_assistant_message": long}));
        assert_eq!(e.last, "é".repeat(300));
        let spaced = format!("{}{}", " \n".repeat(400), "a".repeat(400));
        let e = event(json!({"last_assistant_message": spaced}));
        assert_eq!(e.last, format!(" {}", "a".repeat(299)));
    }

    #[test]
    fn i3_detail() {
        let d = |input: Value| event(json!({"tool_input": input})).detail;
        assert_eq!(d(json!({"file_path": "/a", "command": "ls"})), "ls");
        assert_eq!(d(json!({"description": "d", "pattern": "p"})), "p");
        // jq's //: an empty string or 0 counts, and wins; null and false don't.
        assert_eq!(d(json!({"command": "", "file_path": "x"})), "");
        assert_eq!(d(json!({"command": 0, "file_path": "x"})), "0");
        assert_eq!(d(json!({"command": false, "file_path": "x"})), "x");
        assert_eq!(d(json!({"command": null, "url": "u"})), "u");
        assert_eq!(d(json!({"other": "o"})), "");
        assert_eq!(d(json!({"command": {"b": 2, "a": 1}})), r#"{"b":2,"a":1}"#);
        // Not an object: itself (null and false: nothing).
        assert_eq!(d(json!("raw text")), "raw text");
        assert_eq!(d(json!([1, {"b": 2, "a": 1}])), r#"[1,{"b":2,"a":1}]"#);
        assert_eq!(d(json!(true)), "true");
        assert_eq!(d(json!(null)), "");
        assert_eq!(d(json!(false)), "");
        assert_eq!(event(json!({})).detail, "");
    }

    #[test]
    fn x1_turn_id() {
        assert_eq!(event(json!({"turn_id": "t1"})).turn, "t1");
        assert_eq!(event(json!({"turn_id": null})).turn, "");
        assert_eq!(event(json!({})).turn, "");
        assert_eq!(event(json!({"turn_id": 7})).turn, "7");
    }

    #[test]
    fn x2_fingerprint_in_the_client() {
        let e = event(
            json!({"tool_name": "Bash", "tool_input": {"command": "x", "description": "why"}}),
        );
        // agent-codex's call_key: sha256(json.dumps(["Bash", {"command": "x"}], ...))
        assert_eq!(
            e.fingerprint,
            "a0fc644f7c1a418fc9e522bdf46f3dbd4d8c4904782abb31a02961b1bd228e76"
        );
        // Missing: "" and {} as in agent-codex.
        assert_eq!(
            event(json!({})).fingerprint,
            fingerprint(&json!(""), &json!({}))
        );
        assert_eq!(
            event(json!({"tool_name": "Bash", "tool_input": null})).fingerprint,
            fingerprint(&json!("Bash"), &Value::Null)
        );
    }

    #[test]
    fn x4_accepted() {
        let accepted = |r: Value| event(json!({"tool_response": r})).accepted;
        assert!(accepted(json!({"accepted": true})));
        assert!(accepted(json!("{\"accepted\":true}")));
        assert!(!accepted(json!({"accepted": false})));
        assert!(!accepted(json!({"accepted": "true"})));
        assert!(!accepted(json!("{}")));
        assert!(!accepted(json!("not json")));
        assert!(!accepted(Value::Null));
        assert!(!event(json!({})).accepted);
    }

    #[test]
    fn x8_no_tool_input_in_the_request() {
        let canary = "canary-command-words";
        let e = event(json!({"tool_name": "Bash", "tool_use_id": "a",
            "tool_input": {"command": "ls", "content": canary, "description": canary},
            "tool_response": {"output": canary}}));
        let sent = serde_json::to_string(&e).unwrap();
        assert!(!sent.contains(canary), "{sent}");
    }

    #[test]
    fn i2_not_an_object() {
        assert!(event_from_json(&json!([1])).is_none());
        assert!(event_from_json(&json!("x")).is_none());
        assert!(event_from_json(&json!(3)).is_none());
        assert_eq!(event_from_json(&json!(null)), Some(Event::default()));
    }
}
