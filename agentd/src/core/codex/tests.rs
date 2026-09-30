//! Codex, by rule id: the pure scenarios of agents/tests/test_codex.py and
//! those of contract §6, through `core::handle` as the daemon calls it.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use serde_json::{Value, json};

use super::*;
use crate::core::{self, Effect, Facts, Input, Kind, Op, Pane, State, Urgency, Visibility};

const NOW: i64 = 1_790_000_000;
const AGENT: u32 = 4242;
const START: u64 = 777;

/// A pane whose options carry over from event to event, as tmux keeps them,
/// and the session's rollout as a list of records.
struct Sim {
    state: State,
    opts: BTreeMap<String, String>,
    records: Vec<Value>,
    turn: String,
    vis: Visibility,
    procs: Option<Rc<FakeProcs>>,
}

struct Run(Vec<Effect>);

impl Run {
    fn ops(&self) -> &[Op] {
        match self.0.first() {
            Some(Effect::Options(ops)) => ops,
            _ => &[],
        }
    }
    fn opt(&self, name: &str) -> Option<Option<&str>> {
        self.ops().iter().rev().find_map(|op| match op {
            Op::Set(n, v) if *n == name => Some(Some(v.as_str())),
            Op::Unset(n) if *n == name => Some(None),
            _ => None,
        })
    }
    fn sounds(&self) -> Vec<&'static str> {
        self.0
            .iter()
            .filter_map(|e| match e {
                Effect::Sound(s) => Some(*s),
                _ => None,
            })
            .collect()
    }
    fn notifies(&self) -> Vec<(Urgency, String)> {
        self.0
            .iter()
            .filter_map(|e| match e {
                Effect::Notify { urgency, body, .. } => Some((*urgency, body.clone())),
                _ => None,
            })
            .collect()
    }
}

impl Sim {
    fn new() -> Sim {
        let mut sim = Sim {
            state: State::default(),
            opts: BTreeMap::new(),
            records: Vec::new(),
            turn: "one".into(),
            vis: Visibility::Away,
            procs: None,
        };
        sim.hook("SessionStart", json!({"source": "startup"}));
        sim.hook("UserPromptSubmit", json!({}));
        sim
    }

    fn opt(&self, name: &str) -> &str {
        self.opts.get(name).map_or("", String::as_str)
    }

    fn pane(&self) -> Pane {
        let o = |n: &str| self.opt(n).to_string();
        Pane {
            state: o("@agent_state"),
            since: o("@agent_since"),
            prev: o("@agent_prev"),
            needs_id: o("@agent_needs_id"),
            needs: o("@agent_needs"),
            tool: o("@agent_tool"),
            tests_sound_at: o("@agent_tests_sound_at"),
            sid: o("@agent_session"),
            session: "api".into(),
            title: "✳ Fix CSV".into(),
            space: "work".into(),
            muted: false,
            agent: o("@agent"),
            turn: o("@agent_turn"),
            transcript: o("@agent_transcript"),
            agent_pid: o("@agent_pid"),
            agent_pid_start: o("@agent_pid_start"),
            bg: o("@agent_bg"),
            bg_watch: o("@agent_bg_watch"),
        }
    }

    /// The rollout as the daemon reads it on: from where the pane's
    /// bookkeeping left off (the offset counts records here).
    fn rollout(&self, pane: &Pane) -> Rollout {
        let mut r = rollout_base(&self.state, "%1", pane);
        let file = FileId {
            path: "/r.jsonl".into(),
            dev: 1,
            ino: 1,
        };
        if r.must_restart(&file, self.records.len() as u64) {
            r.restart(file);
        }
        for record in &self.records[r.offset as usize..] {
            r.apply(record);
        }
        r.offset = self.records.len() as u64;
        r
    }

    fn event(&self, name: &str, fields: Value) -> Event {
        let f = fields.as_object().cloned().unwrap_or_default();
        let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let mut e = Event {
            ev: name.into(),
            sid: f
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("session")
                .into(),
            turn: f
                .get("turn_id")
                .and_then(Value::as_str)
                .unwrap_or(&self.turn)
                .into(),
            tool: s("tool_name"),
            tool_id: s("tool_use_id"),
            source: s("source"),
            agent_id: s("agent_id"),
            agent_type: s("agent_type"),
            last: s("last_assistant_message"),
            transcript: "/r.jsonl".into(),
            accepted: f.get("accepted") == Some(&Value::Bool(true)),
            ..Event::default()
        };
        e.fingerprint = fingerprint(
            &Value::String(e.tool.clone()),
            f.get("tool_input").unwrap_or(&json!({})),
        );
        e
    }

    fn run(&mut self, event: Event, observing: bool) -> Run {
        let pane = self.pane();
        let facts = Facts {
            codex: Some(CodexFacts {
                rollout: self.rollout(&pane),
                agent_start: Some(START),
                procs: self.procs.clone().map(|p| p as Rc<dyn Procs>),
                helpers: "/x/agents/bin".into(),
            }),
            now: NOW,
            host: "box".into(),
            pane,
            vis: Some(self.vis),
            others: Vec::new(),
            bg_shells: 0,
            remind_after: String::new(),
            test_regex: String::new(),
        };
        let input = Input {
            kind: Kind::Codex,
            pane: "%1".into(),
            event,
            config_dir: None,
            agent_pid: AGENT,
            observing,
            replay: false,
        };
        let run = Run(core::handle(&mut self.state, &input, &facts));
        for op in run.ops() {
            match op {
                Op::Set(n, v) => {
                    self.opts.insert(n.to_string(), v.clone());
                }
                Op::Unset(n) => {
                    self.opts.remove(*n);
                }
            }
        }
        run
    }

    fn hook(&mut self, name: &str, fields: Value) -> Run {
        let e = self.event(name, fields);
        self.run(e, false)
    }

    /// What the daemon's observer sends (X5).
    fn observe(&mut self) -> Run {
        let e = Event {
            ev: "CodexReconcile".into(),
            sid: self.opt("@agent_session").into(),
            turn: self.opt("@agent_turn").into(),
            transcript: self.opt("@agent_transcript").into(),
            ..Event::default()
        };
        self.run(e, true)
    }

    fn pre(&mut self, tool: &str, call: &str, input: Value) -> Run {
        self.hook(
            "PreToolUse",
            json!({"tool_name": tool, "tool_use_id": call, "tool_input": input}),
        )
    }
    fn ask(&mut self, tool: &str, input: Value) -> Run {
        self.hook(
            "PermissionRequest",
            json!({"tool_name": tool, "tool_input": input}),
        )
    }
    fn post(&mut self, tool: &str, call: &str) -> Run {
        self.hook(
            "PostToolUse",
            json!({"tool_name": tool, "tool_use_id": call}),
        )
    }
    fn prompt(&mut self, turn: &str) -> Run {
        self.turn = turn.into();
        self.hook("UserPromptSubmit", json!({}))
    }
    fn lifecycle(&mut self, kind: &str, turn: &str, fields: Value) {
        let mut payload = fields.as_object().cloned().unwrap_or_default();
        payload.insert("type".into(), kind.into());
        payload.insert("turn_id".into(), turn.into());
        self.records
            .push(json!({"type": "event_msg", "payload": payload}));
    }
    /// (state, needs, needs id) as the pane shows them.
    fn needs(&self) -> (&str, &str, &str) {
        (
            self.opt("@agent_state"),
            self.opt("@agent_needs"),
            self.opt("@agent_needs_id"),
        )
    }
    fn codex(&self) -> &CodexPane {
        &self.state.codex["%1"]
    }
}

fn fp(tool: &str, input: Value) -> String {
    fingerprint(&Value::String(tool.into()), &input)
}

// --- X1 session and turn ---------------------------------------------------------

#[test]
fn x1_events_of_another_session_are_ignored() {
    let mut sim = Sim::new();
    assert!(
        sim.hook(
            "PreToolUse",
            json!({"session_id": "old", "tool_name": "Bash", "tool_use_id": "a"})
        )
        .0
        .is_empty()
    );
    assert!(
        sim.hook("SessionEnd", json!({"session_id": "old"}))
            .0
            .is_empty()
    );
    assert_eq!(sim.opt("@agent_session"), "session");
}

#[test]
fn x1_a_new_session_resets() {
    let mut sim = Sim::new();
    sim.pre("request_user_input", "q", json!({}));
    assert_eq!(sim.needs().0, "needs");
    let r = sim.hook(
        "SessionStart",
        json!({"session_id": "new", "source": "startup"}),
    );
    assert_eq!(r.opt("@agent_state"), Some(Some("ready")));
    assert_eq!(sim.opt("@agent_session"), "new");
    assert!(sim.codex().pending.is_empty() && sim.codex().calls.is_empty());
}

#[test]
fn x1_events_of_an_old_turn_are_ignored() {
    let mut sim = Sim::new();
    assert!(
        sim.hook(
            "PostToolUse",
            json!({"turn_id": "old", "tool_name": "Bash", "tool_use_id": "a"})
        )
        .0
        .is_empty()
    );
    // No turn id: not stale.
    assert_eq!(
        sim.hook("PostToolUse", json!({"turn_id": "", "tool_name": "Bash"}))
            .opt("@agent_state"),
        Some(Some("working"))
    );
}

#[test]
fn x1_a_new_turn_forgets_calls_and_waits() {
    let mut sim = Sim::new();
    sim.pre("Bash", "a", json!({"command": "ls"}));
    sim.ask("Bash", json!({"command": "ls"}));
    assert_eq!(sim.needs(), ("needs", "permission", "a"));
    sim.prompt("two");
    assert_eq!(sim.needs(), ("working", "", ""));
    assert!(sim.codex().calls.is_empty());
    assert_eq!(sim.opt("@agent_turn"), "two");
}

// --- X2 calls and waits (test_codex.py TransitionTests) --------------------------

#[test]
fn x2_parallel_permission_without_id() {
    let mut sim = Sim::new();
    sim.pre("Bash", "a", json!({"command": "echo A"}));
    sim.pre("Bash", "b", json!({"command": "echo B"}));
    sim.ask(
        "Bash",
        json!({"command": "echo A", "description": "approval"}),
    );
    sim.post("Bash", "b");
    assert_eq!(sim.needs(), ("needs", "permission", "a"));
    assert_eq!(
        sim.codex()
            .pending
            .iter()
            .map(|w| w.id.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
    sim.post("Bash", "a");
    assert_eq!(sim.needs(), ("working", "", ""));
}

#[test]
fn x2_unmatched_permission_is_conservative() {
    let mut sim = Sim::new();
    sim.ask("Bash", json!({"command": "missing pre"}));
    let id = format!(
        "unmatched:{}",
        fp("Bash", json!({"command": "missing pre"}))
    );
    assert_eq!(sim.needs(), ("needs", "permission", id.as_str()));
    sim.post("Bash", "other");
    assert_eq!(sim.needs().0, "needs");
    sim.hook("Interrupt", json!({}));
    assert_eq!(sim.needs(), ("idle", "", ""));
}

#[test]
fn x2_identical_parallel_commands_wait_for_both() {
    let mut sim = Sim::new();
    sim.pre("Bash", "a", json!({"command": "echo same"}));
    sim.pre("Bash", "b", json!({"command": "echo same"}));
    sim.ask("Bash", json!({"command": "echo same"}));
    assert!(sim.needs().2.starts_with("unresolved:"));
    sim.post("Bash", "a");
    assert_eq!(sim.needs().0, "needs");
    sim.post("Bash", "b");
    assert_eq!(sim.needs(), ("working", "", ""));
}

#[test]
fn x2_question_and_other_tool_completion() {
    let mut sim = Sim::new();
    sim.pre("request_user_input", "q", json!({}));
    sim.post("Bash", "other");
    assert_eq!(sim.needs(), ("needs", "question", "q"));
    sim.post("request_user_input", "q");
    assert_eq!(sim.needs(), ("working", "", ""));
}

#[test]
fn x2_fingerprint_is_tool_and_canonical_input() {
    let mut sim = Sim::new();
    sim.pre("Bash", "a", json!({"b": 1, "a": [2, 3]}));
    sim.ask("Bash", json!({"a": [2, 3], "b": 1}));
    assert_eq!(sim.needs().2, "a");
    sim.post("Bash", "a");
    sim.pre("Bash", "c", json!({"command": "x"}));
    sim.ask("shell", json!({"command": "x"})); // another tool: no match
    sim.post("Bash", "c");
    assert_eq!(sim.needs().0, "needs");
}

#[test]
fn x2_the_oldest_wait_shows() {
    let mut sim = Sim::new();
    sim.pre("request_user_input", "q", json!({}));
    sim.pre("Bash", "a", json!({"command": "rm x"}));
    sim.ask("Bash", json!({"command": "rm x"}));
    assert_eq!(sim.needs(), ("needs", "question", "q"));
    sim.post("request_user_input", "q");
    assert_eq!(sim.needs(), ("needs", "permission", "a"));
}

#[test]
fn x2_fingerprint_matches_agent_codex() {
    // sha256(json.dumps([tool, input], sort_keys=True, separators=(",", ":")))
    assert_eq!(
        fp("Bash", json!({"command": "x"})),
        "a0fc644f7c1a418fc9e522bdf46f3dbd4d8c4904782abb31a02961b1bd228e76"
    );
    assert_eq!(
        fp("Bash", json!({"b": 1, "a": [2, 3], "description": "why"})),
        "b8ad3478969cc4c79e5e708482e7ae431dc6043a3efbdafa34df68e052ea2465"
    );
    assert_eq!(
        fp("Bash", json!({"command": "é ✓ 𝄞\u{7f}\u{1}\"\\\n"})),
        "7e64ecb7f763a224c8890107ad39e2e98b68bbfd55db31dda2c9557cb8c3be66"
    );
    assert_eq!(
        fp("Bash", Value::Null),
        "17e8dca493771c965c634edd7313d2a635b1e4b628effe3d149f200bf88dfdea"
    );
    assert_eq!(
        fp("Bash", json!({})),
        "65c1dbd3f66b45199a85e1e228c336f6cf0d00a1874827be8101d8173b319d76"
    );
    assert_eq!(
        fp("", json!({})),
        "74ef87f9f2b6ee5d715f94ab1ca8f154025d32d2baa944d6a013d68024c75d27"
    );
    assert_eq!(
        fp("Bash", json!(1e100)),
        "20cedb2fc854ba0caa0aad56bacb25a18f530aa043ce5984d8e8ae3f220c3ea8"
    );
    assert_eq!(
        fp("Bash", json!([1.5, -2, true, null])),
        "df5597cb38e726101c2de5b61a491fb4b78979cc53e5aa220d4a3e0eb9d60115"
    );
}

// --- X3 state ------------------------------------------------------------------------

#[test]
fn x3_late_result_after_stop_changes_nothing() {
    let mut sim = Sim::new();
    sim.hook("Stop", json!({"last_assistant_message": "done"}));
    assert_eq!(sim.opt("@agent_state"), "done");
    let r = sim.post("Bash", "late");
    assert_eq!(r.opt("@agent_state"), None);
    assert_eq!(sim.opt("@agent_state"), "done");
}

#[test]
fn x3_compaction() {
    let mut sim = Sim::new();
    sim.hook("PreCompact", json!({}));
    assert_eq!(sim.opt("@agent_state"), "compacting");
    assert_eq!(sim.opt("@agent_prev"), "working");
    sim.hook("PostCompact", json!({}));
    assert_eq!(sim.opt("@agent_state"), "working");
    // A compaction restart changes nothing.
    let r = sim.hook("SessionStart", json!({"source": "compact"}));
    assert_eq!(r.opt("@agent_state"), None);
}

#[test]
fn x3_compaction_hides_an_open_wait() {
    let mut sim = Sim::new();
    sim.pre("request_user_input", "q", json!({}));
    sim.hook("PreCompact", json!({}));
    assert_eq!(sim.needs(), ("compacting", "", ""));
    sim.hook("PostCompact", json!({}));
    assert_eq!(sim.needs(), ("needs", "question", "q"));
}

#[test]
fn x3_stop_ends_the_turn_and_its_waits() {
    let mut sim = Sim::new();
    sim.pre("request_user_input", "q", json!({}));
    let r = sim.hook("Stop", json!({"last_assistant_message": "bye"}));
    assert_eq!(sim.needs(), ("done", "", ""));
    assert_eq!(sim.opt("@agent_msg"), "bye");
    assert_eq!(sim.opt("@agent_outcome"), "complete");
    assert!(r.has(&Effect::NotifyClose));
    assert!(sim.codex().pending.is_empty() && sim.codex().terminal);
}

#[test]
fn x3_interrupt() {
    let mut sim = Sim::new();
    sim.pre("Bash", "a", json!({"command": "ls"}));
    sim.hook("PreCompact", json!({}));
    sim.hook("PostCompact", json!({}));
    sim.hook("Interrupt", json!({}));
    assert_eq!(sim.opt("@agent_state"), "idle");
    assert_eq!(sim.opt("@agent_outcome"), "interrupted");
    assert_eq!(sim.opt("@agent_tool"), "");
    assert_eq!(sim.opt("@agent_prev"), "");
}

#[test]
fn x3_stop_in_the_pane_in_front_is_idle() {
    let mut sim = Sim::new();
    sim.vis = Visibility::Visible;
    sim.hook("Stop", json!({}));
    assert_eq!(sim.opt("@agent_state"), "idle");
}

#[test]
fn x3_session_end_clears() {
    let mut sim = Sim::new();
    sim.pre("request_user_input", "q", json!({}));
    let r = sim.hook("SessionEnd", json!({}));
    assert!(sim.opts.is_empty(), "{:?}", sim.opts);
    assert!(r.has(&Effect::NotifyClose) && r.has(&Effect::RemindCancel));
    assert!(!sim.state.codex.contains_key("%1"));
    assert!(!r.has(&Effect::Watch));
}

#[test]
fn x3_identity_has_no_profile() {
    let sim = Sim::new();
    assert_eq!(sim.opt("@agent"), "codex");
    assert_eq!(sim.opt("@agent_session"), "session");
    assert_eq!(sim.opt("@agent_transcript"), "/r.jsonl");
    assert!(!sim.opts.contains_key("@agent_profile"));
}

#[test]
fn x3_sounds_and_notifications_are_shared() {
    let mut sim = Sim::new();
    let r = sim.pre("request_user_input", "q", json!({}));
    assert_eq!(r.sounds(), ["report-in"]);
    assert_eq!(
        r.notifies(),
        [(Urgency::Critical, "Has a question for you".to_string())]
    );
    assert!(r.0.iter().any(|e| matches!(e, Effect::RemindArm { .. })));
    let r = sim.ask("Bash", json!({"command": "ls"}));
    assert!(r.sounds().is_empty(), "already waiting");
}

// --- X4 question sent ----------------------------------------------------------------

#[test]
fn x4_question_sent() {
    let mut sim = Sim::new();
    sim.hook(
        "PostToolUse",
        json!({"tool_name": "request_user_input_async", "tool_use_id": "q", "accepted": true}),
    );
    assert_eq!(sim.opt("@agent_question"), "sent");
    assert_eq!(sim.opt("@agent_state"), "working"); // an acknowledgement, not an answer
    sim.hook("Stop", json!({}));
    assert_eq!(sim.opt("@agent_question"), "sent");
    sim.prompt("two");
    assert_eq!(sim.opt("@agent_question"), "");
}

#[test]
fn x4_not_sent_unless_accepted() {
    let mut sim = Sim::new();
    sim.hook(
        "PostToolUse",
        json!({"tool_name": "request_user_input_async", "tool_use_id": "q"}),
    );
    assert_eq!(sim.opt("@agent_question"), "");
    sim.hook(
        "PostToolUse",
        json!({"tool_name": "Bash", "tool_use_id": "q", "accepted": true}),
    );
    assert_eq!(sim.opt("@agent_question"), "");
}

// --- X6 options ----------------------------------------------------------------------

#[test]
fn x6_turn_pid_and_outcome() {
    let mut sim = Sim::new();
    assert_eq!(sim.opt("@agent_turn"), "one");
    assert_eq!(sim.opt("@agent_pid"), AGENT.to_string());
    assert_eq!(sim.opt("@agent_pid_start"), START.to_string());
    assert_eq!(sim.opt("@agent_outcome"), "");
    sim.hook("Stop", json!({}));
    assert_eq!(sim.opt("@agent_outcome"), "complete");
    sim.prompt("two");
    assert_eq!(
        (sim.opt("@agent_turn"), sim.opt("@agent_outcome")),
        ("two", "")
    );
    sim.hook("Interrupt", json!({}));
    assert_eq!(sim.opt("@agent_outcome"), "interrupted");
}

#[test]
fn x6_collaboration_of_the_current_turn() {
    let mut sim = Sim::new();
    sim.lifecycle(
        "task_started",
        "one",
        json!({"collaboration_mode_kind": "plan"}),
    );
    sim.pre("Bash", "a", json!({}));
    assert_eq!(sim.opt("@agent_collaboration"), "plan");
    sim.prompt("two"); // no task_started for "two" yet
    assert_eq!(sim.opt("@agent_collaboration"), "");
}

#[test]
fn x6_a1_subagent_turn_does_not_replace_the_parent() {
    let mut sim = Sim::new();
    let r = sim.hook(
        "SubagentStart",
        json!({"agent_id": "child", "agent_type": "worker", "turn_id": "child-turn"}),
    );
    assert_eq!(sim.opt("@agent_subs"), "1");
    assert_eq!(r.opt("@agent_turn"), None);
    assert!(!r.has(&Effect::Watch));
    assert_eq!(sim.codex().turn, "one");
    sim.hook(
        "SubagentStop",
        json!({"agent_id": "child", "turn_id": "child-turn"}),
    );
    assert_eq!(sim.opt("@agent_subs"), "0");
}

// --- X5 observation --------------------------------------------------------------------

#[test]
fn x5_task_complete_is_done() {
    let mut sim = Sim::new();
    sim.lifecycle("task_started", "one", json!({}));
    sim.lifecycle(
        "task_complete",
        "one",
        json!({"last_agent_message": "All   done."}),
    );
    let r = sim.observe();
    assert_eq!(sim.opt("@agent_state"), "done");
    assert_eq!(sim.opt("@agent_msg"), "All done.");
    assert_eq!(sim.opt("@agent_outcome"), "complete");
    assert_eq!(r.sounds(), ["enemy-down"]);
    assert_eq!(r.notifies(), [(Urgency::Normal, "All done.".to_string())]);
    assert!(!r.has(&Effect::Watch), "observations don't start watching");
}

#[test]
fn x5_error_after_many_records() {
    let mut sim = Sim::new();
    sim.lifecycle(
        "task_complete",
        "one",
        json!({"error": {"message": "limit", "codex_error_info": "x"}}),
    );
    sim.records.extend(std::iter::repeat_n(Value::Null, 350));
    let r = sim.observe();
    assert_eq!(sim.opt("@agent_state"), "error");
    assert_eq!(sim.opt("@agent_msg"), "limit");
    assert_eq!(sim.opt("@agent_outcome"), "error");
    assert_eq!(r.sounds(), ["oh-man"]);
    assert_eq!(
        r.notifies(),
        [(Urgency::Critical, "Stopped: limit".to_string())]
    );
    // Not again once in error.
    assert_eq!(sim.observe().opt("@agent_state"), None);
}

#[test]
fn x5_turn_aborted_is_idle() {
    let mut sim = Sim::new();
    sim.pre("Bash", "a", json!({"command": "ls"}));
    sim.lifecycle("turn_aborted", "one", json!({}));
    sim.observe();
    assert_eq!(sim.opt("@agent_state"), "idle");
    assert_eq!(sim.opt("@agent_outcome"), "interrupted");
    assert_eq!(sim.opt("@agent_tool"), "");
}

#[test]
fn x5_old_abort_cannot_finish_a_new_turn() {
    let mut sim = Sim::new();
    sim.prompt("new");
    sim.lifecycle("task_started", "new", json!({}));
    sim.lifecycle("turn_aborted", "one", json!({}));
    sim.observe();
    assert_eq!(sim.opt("@agent_state"), "working");
}

#[test]
fn x5_output_closes_a_call_and_its_wait() {
    let mut sim = Sim::new();
    sim.pre("Bash", "a", json!({"command": "ls"}));
    sim.ask("Bash", json!({"command": "ls"}));
    assert_eq!(sim.needs().0, "needs");
    sim.lifecycle("task_started", "one", json!({}));
    sim.records.push(json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "a"}}));
    let r = sim.observe();
    assert_eq!(sim.needs(), ("working", "", ""));
    assert!(r.has(&Effect::NotifyClose));
}

#[test]
fn x5_needs_without_a_wait_is_working() {
    let mut sim = Sim::new();
    sim.lifecycle("task_started", "one", json!({}));
    sim.opts.insert("@agent_state".into(), "needs".into()); // P2: set from outside
    sim.observe();
    assert_eq!(sim.opt("@agent_state"), "working");
}

#[test]
fn x5_no_rollout_is_not_completion() {
    let mut sim = Sim::new();
    let r = sim.observe();
    assert_eq!(r.opt("@agent_state"), None);
    assert_eq!(sim.opt("@agent_state"), "working");
}

#[test]
fn x5_observation_of_a_pane_without_codex_is_skipped() {
    let mut sim = Sim::new();
    sim.opts.insert("@agent".into(), "claude".into());
    assert!(sim.observe().0.is_empty());
}

#[test]
fn x5_rollout_records() {
    let mut r = Rollout::default();
    r.apply(&json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "new", "collaboration_mode_kind": "plan"}}));
    r.apply(&json!({"type": "event_msg", "payload": {"type": "turn_aborted", "turn_id": "old"}}));
    assert_eq!(
        (r.turn.as_deref(), r.status.as_str()),
        (Some("new"), "busy")
    );
    r.apply(&json!(null));
    r.apply(&json!({"type": "event_msg", "payload": "odd"}));
    r.apply(&json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "new", "error": "boom"}}));
    assert_eq!(
        (
            r.status.as_str(),
            r.error.as_str(),
            r.collaboration.as_str()
        ),
        ("error", "boom", "plan")
    );
    for i in 0..300 {
        r.apply(&json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": format!("c{i}")}}));
    }
    assert_eq!(r.completed_calls.len(), 256);
    assert_eq!(r.completed_calls[0], "c44");
}

#[test]
fn x5_rollout_restarts_on_another_file_or_truncation() {
    let a = FileId {
        path: "/r".into(),
        dev: 1,
        ino: 1,
    };
    let mut r = Rollout::default();
    assert!(r.must_restart(&a, 0));
    r.restart(a.clone());
    r.offset = 100;
    r.turn = Some("one".into());
    assert!(!r.must_restart(&a, 100));
    assert!(r.must_restart(&a, 99), "truncated");
    let b = FileId {
        ino: 2,
        ..a.clone()
    };
    assert!(r.must_restart(&b, 500), "rotated");
    r.restart(b);
    assert_eq!((r.offset, r.turn.clone()), (0, None));
}

#[test]
fn x5_keep_watching() {
    let codex = |terminal: bool, status: &str| CodexPane {
        turn: "one".into(),
        terminal,
        rollout: Rollout {
            turn: Some("one".into()),
            status: status.into(),
            ..Rollout::default()
        },
        ..CodexPane::default()
    };
    let after = |state: &str, bg: &str| After {
        agent: "codex".into(),
        session: "s".into(),
        state: state.into(),
        bg: bg.into(),
        agent_pid: "1".into(),
    };
    let mut session = "s".to_string();
    let mut since = None;
    // Running: go on.
    assert!(keep_watching(
        &mut session,
        &after("working", ""),
        Some(&codex(false, "busy")),
        0,
        &mut since
    ));
    // Over per rollout, nothing left running: stop; with a command left: go on.
    assert!(!keep_watching(
        &mut session,
        &after("done", ""),
        Some(&codex(true, "complete")),
        0,
        &mut since
    ));
    assert!(keep_watching(
        &mut session,
        &after("done", "1"),
        Some(&codex(true, "complete")),
        0,
        &mut since
    ));
    // Stop without the rollout saying so: 10 s later.
    let mut since = None;
    let c = codex(true, "busy");
    assert!(keep_watching(
        &mut session,
        &after("done", ""),
        Some(&c),
        1_000,
        &mut since
    ));
    assert!(keep_watching(
        &mut session,
        &after("done", ""),
        Some(&c),
        10_999,
        &mut since
    ));
    assert!(!keep_watching(
        &mut session,
        &after("done", ""),
        Some(&c),
        11_000,
        &mut since
    ));
    // ready, gone, another agent: stop; another session: follow it.
    assert!(!keep_watching(
        &mut session,
        &after("ready", ""),
        Some(&codex(false, "busy")),
        0,
        &mut None
    ));
    let gone = After::default();
    assert!(!keep_watching(&mut session, &gone, None, 0, &mut None));
    assert!(keep_watching(
        &mut session,
        &After {
            session: "t".into(),
            ..after("working", "")
        },
        None,
        0,
        &mut None
    ));
    assert_eq!(session, "t");
}

#[test]
fn x5_after_applies_the_writes() {
    let pane = Pane {
        agent: "codex".into(),
        sid: "s".into(),
        state: "working".into(),
        bg: "2".into(),
        ..Pane::default()
    };
    let ops = [
        Op::Set("@agent_state", "done".into()),
        Op::Unset("@agent_bg"),
    ];
    let a = After::from(&pane, &ops);
    assert_eq!(
        (a.state.as_str(), a.bg.as_str(), a.session.as_str()),
        ("done", "", "s")
    );
}

// --- X7 background commands -------------------------------------------------------------

#[derive(Default)]
struct FakeProcs {
    table: HashMap<u32, ProcInfo>,
    env: HashMap<u32, Vec<String>>,
    argv: HashMap<u32, Vec<String>>,
}

impl FakeProcs {
    fn add(&mut self, pid: u32, ppid: u32, sid: u32, thread: Option<&str>, argv: &[&str]) {
        self.table.insert(
            pid,
            ProcInfo {
                ppid,
                sid,
                start: pid as u64 * 10,
            },
        );
        let env = thread
            .map(|t| vec![format!("CODEX_THREAD_ID={t}")])
            .unwrap_or_default();
        self.env.insert(pid, env);
        self.argv
            .insert(pid, argv.iter().map(|a| a.to_string()).collect());
    }
}

impl Procs for FakeProcs {
    fn table(&self) -> &HashMap<u32, ProcInfo> {
        &self.table
    }
    fn stat(&self, pid: u32) -> Option<ProcInfo> {
        self.table.get(&pid).copied()
    }
    fn environ(&self, pid: u32) -> Option<Vec<String>> {
        self.env.get(&pid).cloned()
    }
    fn argv(&self, pid: u32) -> Option<Vec<String>> {
        self.argv.get(&pid).cloned()
    }
}

/// The agent (sid 10), an MCP-like child in its session, one command tree.
fn procs() -> FakeProcs {
    let mut p = FakeProcs::default();
    p.add(AGENT, 1, 10, None, &["codex"]);
    p.add(500, AGENT, 10, Some("s"), &["mcp"]);
    p.add(
        600,
        AGENT,
        600,
        Some("s"),
        &["/bin/sh", "-c", "sleep 300 & wait"],
    );
    p.add(601, 600, 600, Some("s"), &["sleep", "300"]);
    p
}

#[test]
fn x7_a_command_tree_counts_an_mcp_does_not() {
    let groups = command_roots(AGENT, "s", &[], &procs(), "/x/bin");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].keys().copied().collect::<Vec<_>>(), [600, 601]);
}

#[test]
fn x7_a_tree_outlives_its_leader() {
    let mut p = procs();
    let groups = command_roots(AGENT, "s", &[], &p, "/x/bin");
    // The leader exits; its child is reparented to init.
    p.table.remove(&600);
    p.table.get_mut(&601).unwrap().ppid = 1;
    let groups = command_roots(AGENT, "s", &groups, &p, "/x/bin");
    assert_eq!(groups.len(), 1);
    // The same pid with another start time is another process.
    p.table.get_mut(&601).unwrap().start = 1;
    assert!(command_roots(AGENT, "s", &groups, &p, "/x/bin").is_empty());
}

#[test]
fn x7_nested_trees_count_once() {
    let mut p = procs();
    p.add(602, 601, 602, Some("s"), &["sleep", "300"]); // setsid inside the tree
    p.add(700, AGENT, 700, Some("s"), &["sh"]);
    assert_eq!(command_roots(AGENT, "s", &[], &p, "/x/bin").len(), 2);
}

#[test]
fn x7_what_does_not_count() {
    let mut p = FakeProcs::default();
    p.add(AGENT, 1, 10, None, &["codex"]);
    p.add(600, AGENT, 600, Some("another-thread"), &["sh"]);
    p.add(
        601,
        AGENT,
        601,
        Some("s"),
        &["/tmp/x/codex-code-mode-host", "300"],
    );
    p.add(602, AGENT, 602, Some("s"), &["/x/bin/agent-bgwatch", "%1"]);
    p.add(603, AGENT, 603, None, &["sh"]);
    assert!(command_roots(AGENT, "s", &[], &p, "/x/bin").is_empty());
    // Unreadable environment: skipped too.
    p.add(604, AGENT, 604, Some("s"), &["sh"]);
    p.env.remove(&604);
    assert!(command_roots(AGENT, "s", &[], &p, "/x/bin").is_empty());
}

#[test]
fn x7_counted_at_the_end_of_a_turn() {
    for event in ["Stop", "Interrupt"] {
        let mut sim = Sim::new();
        let mut p = procs();
        for (pid, thread) in [(500, "session"), (600, "session"), (601, "session")] {
            p.env.insert(pid, vec![format!("CODEX_THREAD_ID={thread}")]);
        }
        sim.procs = Some(Rc::new(p));
        // Not on every hook.
        sim.pre("Bash", "a", json!({}));
        assert_eq!(sim.opt("@agent_bg"), "");
        let r = sim.hook(event, json!({}));
        assert_eq!(sim.opt("@agent_bg"), "1", "{event}");
        if event == "Stop" {
            assert_eq!(r.notifies()[0].1, "Finished · 1 shell still running");
            assert!(
                !r.0.iter().any(|e| matches!(e, Effect::Bgwatch { .. })),
                "Claude only"
            );
        }
    }
}

// --- X8 privacy ---------------------------------------------------------------------------

#[test]
fn x8_bookkeeping_keeps_no_content() {
    let canary = "canary-words";
    let mut sim = Sim::new();
    sim.pre("Bash", "a", json!({"command": format!("echo {canary}")}));
    sim.ask(
        "Bash",
        json!({"command": format!("echo {canary}"), "description": canary}),
    );
    sim.lifecycle(
        "task_complete",
        "one",
        json!({"last_agent_message": canary, "error": {"message": canary}}),
    );
    sim.observe();
    assert_eq!(sim.opt("@agent_msg"), canary, "shown, but not kept");
    let saved = serde_json::to_string(&sim.state).unwrap();
    assert!(!saved.contains(canary), "{saved}");
}

// --- watching ------------------------------------------------------------------------------

#[test]
fn x5_hooks_ask_for_observation() {
    let mut sim = Sim::new();
    assert!(sim.pre("Bash", "a", json!({})).has(&Effect::Watch));
    assert!(sim.hook("Stop", json!({})).has(&Effect::Watch));
    // Stale events are ignored entirely.
    assert!(
        !sim.hook("Stop", json!({"session_id": "x"}))
            .has(&Effect::Watch)
    );
}

impl Run {
    fn has(&self, effect: &Effect) -> bool {
        self.0.contains(effect)
    }
}
