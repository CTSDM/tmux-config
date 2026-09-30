//! `agentd hook claude|codex` (contract §1): reads the event, keeps what the
//! contract uses, and hands it to the daemon with the parent chain. Never
//! prints, always exits 0.

use std::collections::BTreeMap;
use std::env;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::client;
use crate::core::codex::fingerprint;
use crate::core::{Event, Kind};
use crate::identity;
use crate::parked;
use crate::procfs;
use crate::proto::{HookRequest, Link, Parked, Request, VERSION};
use crate::remote::{self, frame};

/// A bigger payload is cut here; the fields we keep are short anyway.
const MAX_PAYLOAD: u64 = 64 << 20;
/// Parent chain sent for I4 (which looks at 13).
const MAX_CHAIN: usize = 16;
/// Bash's hook in the checkout this binary was built from: where the rollback
/// switch sends the events (`@agents_bin` would cost a tmux call). If the
/// checkout moved, the usual place of the config.
const BASH_HOOK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../agents/bin/agent-hook");
const BASH_HOOK_USUAL: &str = ".config/tmux/agents/bin/agent-hook";
/// How long to wait for a daemon that has to be started first.
const START_BUDGET: Duration = Duration::from_millis(300);
/// The ack comes after one tmux read and one write. A daemon stuck longer
/// (tmux not answering) delays the agent at most this; reconciliation
/// repairs what is lost.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

pub fn run(kind: Option<Kind>) {
    let started = SystemTime::now();
    let mut payload = Vec::new();
    // Read it whatever happens, so the agent's write never fails.
    let _ = io::stdin()
        .lock()
        .take(MAX_PAYLOAD)
        .read_to_end(&mut payload);
    // I1: exactly claude or codex, inside tmux; I5: or a parked Claude
    // session; I6: or held by `agentd hold`, for a remote pane.
    let Some(kind) = kind else {
        return;
    };
    if env::var_os("TMUX").is_none_or(|v| v.is_empty())
        && let Some(hold) = env::var_os(remote::HOLD_VAR).filter(|v| !v.is_empty())
    {
        if !identity::off() {
            held(kind, &payload, started, Path::new(&hold));
        }
        return;
    }
    let (tmux, pane, parked) = match (
        env::var_os("TMUX").filter(|v| !v.is_empty()),
        env::var("TMUX_PANE").ok().filter(|v| !v.is_empty()),
    ) {
        (Some(tmux), Some(pane)) => (tmux, pane, None),
        _ if kind == Kind::Claude
            && env::var("CLAUDE_CODE_SESSION_KIND").as_deref() == Ok("bg") =>
        {
            let Some((viewer, parked)) = parked() else {
                return;
            };
            (viewer.tmux.into(), viewer.pane, Some(parked))
        }
        _ => return,
    };
    if identity::off() {
        // Bash's hook has no I5: it would drop the event anyway.
        if parked.is_none() {
            bash(kind, &payload);
        }
        return;
    }
    // jq reads invalid UTF-8 as U+FFFD; so do we.
    let Ok(json) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&payload)) else {
        return;
    };
    let Some(event) = event_from_json(&json) else {
        return;
    };
    let request = Request::Hook(Box::new(HookRequest {
        parked,
        ..request(kind, pane, event, started)
    }));
    let Some(paths) = client::paths(&tmux) else {
        return;
    };
    // No daemon after the budget: give up, reconciliation repairs it.
    if let Some(stream) = client::connect_or_start(&paths, START_BUDGET) {
        let _ = client::call(stream, &request, REPLY_TIMEOUT);
    }
}

/// The request for an event, with the hook's parent chain.
fn request(kind: Kind, pane: String, event: Event, started: SystemTime) -> HookRequest {
    let mut agent_env = BTreeMap::new();
    if let Ok(dir) = env::var("CLAUDE_CONFIG_DIR") {
        agent_env.insert("CLAUDE_CONFIG_DIR".to_string(), dir);
    }
    HookRequest {
        v: VERSION,
        kind,
        pane,
        event,
        chain: links(std::os::unix::process::parent_id()),
        env: agent_env,
        t: started
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        parked: None,
        remote: None,
    }
}

/// I6: a held agent's event goes to its holder, which checks I4 against the
/// held program and passes it to the remote pane (remote/hold.rs). No pane
/// here: the local end knows it.
fn held(kind: Kind, payload: &[u8], started: SystemTime, hold: &Path) {
    let Ok(json) = serde_json::from_str::<Value>(&String::from_utf8_lossy(payload)) else {
        return;
    };
    let Some(event) = event_from_json(&json) else {
        return;
    };
    let Ok(body) = serde_json::to_vec(&request(kind, String::new(), event, started)) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect(hold) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(REPLY_TIMEOUT));
    let _ = stream.set_read_timeout(Some(REPLY_TIMEOUT));
    if frame::write(&mut stream, frame::HOOK, &body).is_ok() {
        let _ = frame::read(&mut stream);
    }
}

/// `pid` and its parents, as I4 reads them.
fn links(pid: u32) -> Vec<Link> {
    procfs::chain(pid, MAX_CHAIN)
        .into_iter()
        .map(|s| (s.pid, s.comm, s.starttime))
        .collect()
}

/// I5: the pane that shows this parked session, from Claude Code's registry.
fn parked() -> Option<(parked::Viewer, Parked)> {
    let config = parked::config_dir(env::var("CLAUDE_CONFIG_DIR").ok())?;
    let chain: Vec<u32> = procfs::chain(std::os::unix::process::parent_id(), MAX_CHAIN)
        .iter()
        .map(|s| s.pid)
        .collect();
    let (agent, job) = parked::background(&config, &chain)?;
    let viewer = parked::viewer(&config, &job)?;
    let parked = Parked {
        agent,
        viewer: links(viewer.pid),
    };
    Some((viewer, parked))
}

/// The rollback switch: the event goes to bash's `agent-hook`, as it came.
fn bash(kind: Kind, payload: &[u8]) {
    let usual = env::var_os("HOME").map(|h| Path::new(&h).join(BASH_HOOK_USUAL));
    let Some(hook) = [Some(PathBuf::from(BASH_HOOK)), usual]
        .into_iter()
        .flatten()
        .find(|p| p.exists())
    else {
        crate::debug::log(&format!(
            "agentd.off: no bash agent-hook at {BASH_HOOK} or ~/{BASH_HOOK_USUAL}; event dropped"
        ));
        return;
    };
    let Ok(mut child) = Command::new(hook)
        .arg(kind.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload);
    }
    let _ = child.wait();
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
fn line(v: Option<&Value>) -> String {
    match v {
        None => String::new(),
        Some(Value::String(s)) => crate::core::line(s),
        Some(other) => crate::core::line(&other.to_string()),
    }
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
