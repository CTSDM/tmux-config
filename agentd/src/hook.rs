//! `agentd hook claude|codex` (contract §1): reads the event, keeps what the
//! contract uses, and hands it to the daemon with the parent chain. Never
//! prints, always exits 0.

use std::collections::BTreeMap;
use std::env;
use std::io::{self, Read};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::client;
use crate::core::{Event, Kind};
use crate::procfs;
use crate::proto::{HookRequest, Request, VERSION};

/// Cut of every field (I3), in characters.
const MAX_CHARS: usize = 300;
/// A bigger payload is cut here; the fields we keep are short anyway.
const MAX_PAYLOAD: u64 = 64 << 20;
/// Parent chain sent for I4 (which looks at 13).
const MAX_CHAIN: usize = 16;
/// How long to wait for a daemon that has to be started first.
const START_BUDGET: Duration = Duration::from_millis(300);
/// The ack comes after one tmux read and one write.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

pub fn run(kind: Option<Kind>) {
    let started = SystemTime::now();
    let mut payload = Vec::new();
    // Read it whatever happens, so the agent's write never fails.
    let _ = io::stdin()
        .lock()
        .take(MAX_PAYLOAD)
        .read_to_end(&mut payload);
    // I1: exactly claude or codex, inside tmux.
    let (Some(kind), Some(tmux), Some(pane)) = (
        kind,
        env::var_os("TMUX").filter(|v| !v.is_empty()),
        env::var("TMUX_PANE").ok().filter(|v| !v.is_empty()),
    ) else {
        return;
    };
    // jq reads invalid UTF-8 as U+FFFD; so do we.
    let Ok(json) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&payload)) else {
        return;
    };
    let Some(event) = event_from_json(&json) else {
        return;
    };
    let chain = procfs::chain(std::os::unix::process::parent_id(), MAX_CHAIN)
        .into_iter()
        .map(|s| (s.pid, s.comm, s.starttime))
        .collect();
    let mut agent_env = BTreeMap::new();
    if let Ok(dir) = env::var("CLAUDE_CONFIG_DIR") {
        agent_env.insert("CLAUDE_CONFIG_DIR".to_string(), dir);
    }
    let request = Request::Hook(Box::new(HookRequest {
        v: VERSION,
        kind,
        pane,
        event,
        chain,
        env: agent_env,
        t: started
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    }));
    let Some(paths) = client::paths(&tmux) else {
        return;
    };
    // No daemon after the budget: give up, reconciliation repairs it.
    if let Some(stream) = client::connect_or_start(&paths, START_BUDGET) {
        let _ = client::call(stream, &request, REPLY_TIMEOUT);
    }
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
    let field = |key: &str| line(present(obj.get(key)));
    let detail = match present(obj.get("tool_input")) {
        None => String::new(),
        Some(Value::Object(input)) => line(
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
        Some(other) => line(Some(other)),
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
    })
}

/// jq's `//`: null, false and missing are absent; everything else counts,
/// "" and 0 included.
fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

/// `tostring | gsub("\\s+"; " ") | .[0:300]` for a present value, "" else.
fn line(v: Option<&Value>) -> String {
    let text = match v {
        None => return String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
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
    fn i2_not_an_object() {
        assert!(event_from_json(&json!([1])).is_none());
        assert!(event_from_json(&json!("x")).is_none());
        assert!(event_from_json(&json!(3)).is_none());
        assert_eq!(event_from_json(&json!(null)), Some(Event::default()));
    }
}
