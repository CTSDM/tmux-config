//! Codex (contract §6): the per-pane bookkeeping of `agents/bin/agent-codex`
//! at 885bb9b (`transition()`, `prepare()`, `read_rollout()`,
//! `command_roots()`, the end test of `watch()`), without its I/O: the daemon
//! reads the rollout and /proc and hands them in.
//!
//! X8: nothing here that is saved holds content. Calls and waits are kept by
//! fingerprint, the rollout by file identity, offset, turn and call ids;
//! its last message and error text live in memory only.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::text::line;
use super::{Event, Op, Pane, State};

/// A command tree (X7): its processes, pid → start time.
pub type Group = BTreeMap<u32, u64>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitKind {
    Permission,
    Question,
}

impl WaitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitKind::Permission => "permission",
            WaitKind::Question => "question",
        }
    }
}

/// An open wait (X2): by call id, or `unresolved:`/`unmatched:` + fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    pub id: String,
    pub kind: WaitKind,
    pub key: String,
}

/// Which file the rollout reader is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    pub path: String,
    pub dev: u64,
    pub ino: u64,
}

/// What the session's rollout said so far (X5), and where reading goes on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rollout {
    pub file: Option<FileId>,
    pub offset: u64,
    /// `None` until a lifecycle record names one.
    pub turn: Option<String>,
    /// "", `busy`, `complete`, `aborted` or `error`.
    pub status: String,
    pub collaboration: String,
    /// The last `function_call_output` call ids (256 at most).
    pub completed_calls: Vec<String>,
    /// `last_agent_message` of the turn's end: in memory only (X8).
    #[serde(skip)]
    pub message: String,
    /// The error's message: in memory only (X8).
    #[serde(skip)]
    pub error: String,
}

const COMPLETED_CALLS: usize = 256;

impl Rollout {
    /// Reading starts over: another file, or this one got shorter.
    pub fn restart(&mut self, file: FileId) {
        *self = Rollout {
            file: Some(file),
            ..Rollout::default()
        };
    }

    /// Whether the reader must restart for this file of this size.
    pub fn must_restart(&self, file: &FileId, size: u64) -> bool {
        self.file.as_ref() != Some(file) || size < self.offset
    }

    /// One complete JSONL record.
    pub fn apply(&mut self, record: &Value) {
        let Some(record) = record.as_object() else {
            return;
        };
        let empty = serde_json::Map::new();
        let data = match record.get("payload") {
            None => &empty,
            Some(Value::Object(data)) => data,
            Some(_) => return,
        };
        let kind = data.get("type").and_then(Value::as_str);
        match record.get("type").and_then(Value::as_str) {
            Some("event_msg") => match kind {
                Some("task_started") => {
                    self.turn = Some(text(data.get("turn_id")));
                    self.status = "busy".into();
                    self.collaboration = text(data.get("collaboration_mode_kind"));
                    self.error.clear();
                }
                Some(end @ ("task_complete" | "turn_aborted")) => {
                    let turn = text(data.get("turn_id"));
                    // An old turn's end cannot finish the new one.
                    if !turn.is_empty()
                        && self
                            .turn
                            .as_deref()
                            .is_some_and(|t| !t.is_empty() && t != turn)
                    {
                        return;
                    }
                    self.turn = Some(turn);
                    self.status = if end == "turn_aborted" {
                        "aborted"
                    } else {
                        "complete"
                    }
                    .into();
                    self.message = match data.get("last_agent_message") {
                        Some(Value::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    if let Some(error) = truthy(data.get("error")) {
                        self.status = "error".into();
                        self.error = error_message(error);
                    }
                }
                _ => {}
            },
            Some("response_item") if kind == Some("function_call_output") => {
                if let Some(Value::String(call)) = truthy(data.get("call_id"))
                    && !self.completed_calls.contains(call)
                {
                    self.completed_calls.push(call.clone());
                    let extra = self.completed_calls.len().saturating_sub(COMPLETED_CALLS);
                    self.completed_calls.drain(..extra);
                }
            }
            _ => {}
        }
    }
}

/// `error.get("message", "Unknown error")` for an object, else the error.
fn error_message(error: &Value) -> String {
    match error {
        Value::Object(e) => match e.get("message") {
            None => "Unknown error".into(),
            Some(m) => text(Some(m)),
        },
        other => text(Some(other)),
    }
}

/// A value as text: strings as they are, null or missing as "", others as JSON.
fn text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Python truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    })
}

/// One Codex pane's bookkeeping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexPane {
    pub session: String,
    pub turn: String,
    /// Recorded calls: tool id → fingerprint.
    pub calls: BTreeMap<String, String>,
    /// Open waits, oldest first.
    pub pending: Vec<Wait>,
    /// The turn is over (Stop, Interrupt, SessionEnd, or per rollout).
    pub terminal: bool,
    pub question: bool,
    pub outcome: String,
    /// The state PreCompact saw.
    pub previous: Option<String>,
    pub groups: Vec<Group>,
    pub rollout: Rollout,
}

impl CodexPane {
    fn set_wait(&mut self, id: String, kind: WaitKind, key: String) {
        match self.pending.iter_mut().find(|w| w.id == id) {
            Some(w) => {
                w.kind = kind;
                w.key = key;
            }
            None => self.pending.push(Wait { id, kind, key }),
        }
    }

    fn drop_wait(&mut self, id: &str) {
        self.pending.retain(|w| w.id != id);
    }
}

/// The processes X7 looks at: a snapshot of /proc, and what it takes to read
/// a candidate's environment and command line.
pub trait Procs {
    /// Every live process that is not a zombie.
    fn table(&self) -> &HashMap<u32, ProcInfo>;
    /// Any process, zombies included (the agent's own Unix session).
    fn stat(&self, pid: u32) -> Option<ProcInfo>;
    /// The environment's entries; `None` when unreadable.
    fn environ(&self, pid: u32) -> Option<Vec<String>>;
    /// The command line's arguments; `None` when unreadable.
    fn argv(&self, pid: u32) -> Option<Vec<String>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcInfo {
    pub ppid: u32,
    pub sid: u32,
    pub start: u64,
}

/// X7: the command trees the agent started for this session that still live.
/// Trees seen before keep counting while any of their processes lives, even
/// reparented; a tree inside another counts once.
pub fn command_roots(
    agent: u32,
    session: &str,
    groups: &[Group],
    procs: &dyn Procs,
    helpers: &str,
) -> Vec<Group> {
    let all = procs.table();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, p) in all {
        children.entry(p.ppid).or_default().push(*pid);
    }
    for list in children.values_mut() {
        list.sort_unstable();
    }
    let alive = |pid: u32, start: u64| all.get(&pid).is_some_and(|p| p.start == start);
    // The agent's descendants, and those of every live member of a known tree.
    let mut todo: Vec<u32> = vec![agent];
    for group in groups {
        todo.extend(
            group
                .iter()
                .filter(|(p, s)| alive(**p, **s))
                .map(|(p, _)| *p),
        );
    }
    let mut processes: BTreeMap<u32, ProcInfo> = BTreeMap::new();
    while let Some(pid) = todo.pop() {
        if processes.contains_key(&pid) {
            continue;
        }
        let Some(p) = all.get(&pid) else { continue };
        processes.insert(pid, *p);
        todo.extend(children.get(&pid).into_iter().flatten());
    }
    let below = |pid: u32, parents: &dyn Fn(u32) -> bool| {
        let mut pid = pid;
        let mut seen = Vec::new();
        while let Some(p) = processes.get(&pid) {
            if seen.contains(&pid) {
                return false;
            }
            if parents(pid) {
                return true;
            }
            seen.push(pid);
            pid = p.ppid;
        }
        false
    };
    let tree_of = |roots: &dyn Fn(u32) -> bool| -> Group {
        processes
            .iter()
            .filter(|(pid, _)| below(**pid, roots))
            .map(|(pid, p)| (*pid, p.start))
            .collect()
    };

    let mut retained: Vec<Group> = Vec::new();
    for group in groups {
        let live: Vec<u32> = group
            .iter()
            .filter(|(p, s)| alive(**p, **s))
            .map(|(p, _)| *p)
            .collect();
        if !live.is_empty() {
            retained.push(tree_of(&|p| live.contains(&p)));
        }
    }
    let Some(root) = procs.stat(agent) else {
        return retained;
    };
    let thread = format!("CODEX_THREAD_ID={session}");
    let helper_prefix = format!("{helpers}/agent-");
    let mut candidates = Vec::new();
    for (pid, p) in &processes {
        let pid = *pid;
        if pid == agent || p.sid == root.sid || p.sid != pid {
            continue;
        }
        if retained.iter().any(|g| below(pid, &|q| g.contains_key(&q))) {
            continue;
        }
        let (Some(env), Some(argv)) = (procs.environ(pid), procs.argv(pid)) else {
            continue;
        };
        if !env.contains(&thread) {
            continue;
        }
        let helper = !helpers.is_empty()
            && argv
                .iter()
                .any(|a| a.starts_with(&helper_prefix) && !a.contains(' '));
        if helper || argv.iter().any(|a| a.ends_with("/codex-code-mode-host")) {
            continue;
        }
        candidates.push(pid);
    }
    for &pid in &candidates {
        let parent = processes[&pid].ppid;
        if candidates
            .iter()
            .any(|&other| other != pid && below(parent, &|q| q == other))
        {
            continue;
        }
        retained.push(tree_of(&|q| q == pid));
    }
    retained
}

/// The pane's Codex facts beyond `Pane`, read with the event.
#[derive(Clone, Default)]
pub struct CodexFacts {
    /// The rollout, read on from `rollout_base`.
    pub rollout: Rollout,
    /// Start time of the agent process, if it lives.
    pub agent_start: Option<u64>,
    /// For X7; needed when `may_need_procs` says so.
    pub procs: Option<std::rc::Rc<dyn Procs>>,
    /// `@agents_bin`: our helpers never count as commands.
    pub helpers: String,
}

impl std::fmt::Debug for CodexFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexFacts")
            .field("rollout", &self.rollout)
            .field("agent_start", &self.agent_start)
            .field("procs", &self.procs.is_some())
            .field("helpers", &self.helpers)
            .finish()
    }
}

/// Where the rollout reader goes on for this pane (the bookkeeping of
/// another session starts over, as `prepare()` does).
pub fn rollout_base(state: &State, pane_id: &str, pane: &Pane) -> Rollout {
    state
        .codex
        .get(pane_id)
        .filter(|c| c.session == pane.sid)
        .map(|c| c.rollout.clone())
        .unwrap_or_default()
}

/// The rollout of the event, else the pane's.
pub fn rollout_path<'a>(event: &'a Event, pane: &'a Pane) -> &'a str {
    if event.transcript.is_empty() {
        &pane.transcript
    } else {
        &event.transcript
    }
}

/// What `prepare()` hands back to the shared hook code.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// The event, maybe turned by the rollout into Stop, StopFailure or
    /// Interrupt.
    pub event: Event,
    /// "" when the state stays.
    pub state: String,
    pub needs: String,
    /// The Codex options (X6), set when not empty and unset otherwise.
    pub options: Vec<(&'static str, String)>,
}

/// `prepare()`: `None` means the event is ignored (another session, a stale
/// turn, an observation of a pane that no longer holds Codex).
pub fn prepare(
    state: &mut State,
    pane_id: &str,
    event: &Event,
    pane: &Pane,
    facts: &CodexFacts,
    agent: u32,
    observing: bool,
) -> Option<Update> {
    if observing && pane.agent != "codex" {
        return None;
    }
    let mut stored = state
        .codex
        .get(pane_id)
        .filter(|c| c.session == pane.sid)
        .cloned()
        .unwrap_or_else(|| CodexPane {
            session: pane.sid.clone(),
            turn: pane.turn.clone(),
            ..CodexPane::default()
        });
    let rollout = facts.rollout.clone();
    if event.ev == "CodexReconcile" && stored.turn.is_empty() {
        // A pane that was running before the observer existed.
        stored.turn = rollout.turn.clone().unwrap_or_default();
    }
    let (new_state, needs, event) = transition(&mut stored, event, pane, &rollout)?;
    stored.rollout = rollout;
    if !event.agent_id.is_empty() {
        // Subagent turns are independent of the parent: nothing is kept.
        return Some(Update {
            event,
            state: new_state,
            needs,
            options: Vec::new(),
        });
    }
    if event.ev == "SessionEnd" {
        state.codex.remove(pane_id);
        return Some(Update {
            event,
            state: new_state,
            needs,
            options: Vec::new(),
        });
    }
    let mut options = vec![
        ("@agent_turn", stored.turn.clone()),
        ("@agent_outcome", stored.outcome.clone()),
        (
            "@agent_question",
            if stored.question { "sent" } else { "" }.to_string(),
        ),
        ("@agent_collaboration", String::new()),
        ("@agent_pid", agent.to_string()),
        (
            "@agent_pid_start",
            facts.agent_start.map(|s| s.to_string()).unwrap_or_default(),
        ),
    ];
    if stored.rollout.turn.as_deref() == Some(stored.turn.as_str()) {
        options[3].1 = stored.rollout.collaboration.clone();
    }
    if !needs.is_empty()
        && let Some(first) = stored.pending.first()
    {
        options.push(("@agent_needs_id", first.id.clone()));
    }
    // Scan only on observations and at the end of a turn, not every hook.
    if matches!(
        event.ev.as_str(),
        "CodexReconcile" | "Stop" | "StopFailure" | "Interrupt"
    ) {
        if let Some(procs) = &facts.procs {
            stored.groups = command_roots(
                agent,
                &stored.session,
                &stored.groups,
                procs.as_ref(),
                &facts.helpers,
            );
        }
        let n = stored.groups.len();
        options.push((
            "@agent_bg",
            if n > 0 { n.to_string() } else { String::new() },
        ));
    }
    state.codex.insert(pane_id.to_string(), stored);
    Some(Update {
        event,
        state: new_state,
        needs,
        options,
    })
}

/// `transition()`: `None` when stale.
fn transition(
    data: &mut CodexPane,
    e: &Event,
    pane: &Pane,
    rollout: &Rollout,
) -> Option<(String, String, Event)> {
    let ev = e.ev.as_str();
    let start = ev == "SessionStart" && e.source != "compact";
    if !data.session.is_empty() && e.sid != data.session && !start {
        return None; // X1: another session
    }
    if start {
        *data = CodexPane {
            session: e.sid.clone(),
            ..CodexPane::default()
        };
    } else if data.session.is_empty() {
        data.session = e.sid.clone();
        data.turn = pane.turn.clone();
        data.calls.clear();
        data.pending.clear();
        data.terminal = false;
        data.question = false;
    }
    if !e.agent_id.is_empty() {
        return Some((String::new(), String::new(), e.clone()));
    }
    if ev == "UserPromptSubmit" {
        if e.turn != data.turn {
            data.turn = e.turn.clone();
            data.calls.clear();
            data.pending.clear();
            data.terminal = false;
        }
        data.question = false; // a new message dismisses the sent question (X4)
        data.terminal = false;
        data.outcome.clear();
    } else if !e.turn.is_empty() && !data.turn.is_empty() && e.turn != data.turn {
        return None; // X1: stale
    } else if !e.turn.is_empty() && data.turn.is_empty() {
        data.turn = e.turn.clone();
    }

    let mut state = "";
    let mut event = e.clone();
    let (tool, call, key) = (e.tool.as_str(), e.tool_id.as_str(), e.fingerprint.as_str());
    match ev {
        "SessionStart" => {
            if start {
                state = "ready";
            }
        }
        "UserPromptSubmit" => state = "working",
        "PreToolUse" => {
            if !call.is_empty() {
                data.calls.insert(call.into(), key.into());
            }
            data.terminal = false;
            data.outcome.clear();
            state = "working";
            if tool == "request_user_input" {
                let id = if call.is_empty() { key } else { call };
                data.set_wait(id.into(), WaitKind::Question, key.into());
            }
        }
        "PermissionRequest" => {
            let id = if call.is_empty() {
                let matches: Vec<&String> = data
                    .calls
                    .iter()
                    .filter(|(_, v)| *v == key)
                    .map(|(c, _)| c)
                    .collect();
                match matches.as_slice() {
                    [one] => (*one).clone(),
                    [] => format!("unmatched:{key}"),
                    _ => format!("unresolved:{key}"),
                }
            } else {
                call.to_string()
            };
            data.set_wait(id, WaitKind::Permission, key.into());
        }
        "PostToolUse" => {
            data.calls.remove(call);
            data.drop_wait(call);
            if tool == "request_user_input_async" && e.accepted {
                data.question = true; // X4
            }
            // A background result may arrive after the turn ended.
            if !data.terminal {
                state = "working";
            }
        }
        "PreCompact" => {
            data.previous = Some(if pane.state.is_empty() {
                "working".into()
            } else {
                pane.state.clone()
            });
            state = "compacting";
        }
        "PostCompact" => {
            let previous = data.previous.take().unwrap_or_else(|| "working".into());
            let restored = if previous == "compacting" {
                "working".to_string()
            } else {
                previous
            };
            return finish(data, restored, event);
        }
        "Stop" | "Interrupt" | "SessionEnd" => {
            data.terminal = true;
            data.pending.clear();
            state = if ev == "Stop" { "done" } else { "idle" };
            data.outcome = if ev == "Interrupt" {
                "interrupted"
            } else {
                "complete"
            }
            .into();
        }
        "CodexReconcile" => {
            if rollout.turn.as_deref() == Some(data.turn.as_str()) {
                for done in &rollout.completed_calls {
                    data.calls.remove(done);
                    data.drop_wait(done);
                }
                let status = rollout.status.as_str();
                if matches!(status, "error" | "aborted" | "complete") {
                    data.terminal = true;
                    data.pending.clear();
                    data.outcome = if status == "aborted" {
                        "interrupted"
                    } else {
                        status
                    }
                    .into();
                    let cur = pane.state.as_str();
                    if status == "error" && cur != "error" {
                        event.ev = "StopFailure".into();
                        event.error = line(&rollout.error);
                        state = "error";
                    } else if status == "aborted" && !matches!(cur, "idle" | "error") {
                        event.ev = "Interrupt".into();
                        state = "idle";
                    } else if status == "complete"
                        && matches!(cur, "working" | "needs" | "compacting")
                    {
                        event.ev = "Stop".into();
                        event.last = line(&rollout.message);
                        state = "done";
                    }
                } else if pane.state == "needs" && data.pending.is_empty() && !data.terminal {
                    state = "working";
                }
            }
        }
        _ => return None,
    }
    finish(data, state.to_string(), event)
}

/// The end of `transition()`: unresolved waits whose calls are all gone go;
/// the oldest open wait shows while the turn runs and nothing compacts.
fn finish(
    data: &mut CodexPane,
    mut state: String,
    event: Event,
) -> Option<(String, String, Event)> {
    let calls: Vec<String> = data.calls.values().cloned().collect();
    data.pending
        .retain(|w| !(w.id.starts_with("unresolved:") && !calls.contains(&w.key)));
    let mut needs = String::new();
    if let Some(oldest) = data.pending.first()
        && !data.terminal
        && state != "compacting"
    {
        state = "needs".into();
        needs = oldest.kind.as_str().into();
    }
    Some((state, needs, event))
}

/// The pane as an observation left it, for `keep_watching`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct After {
    pub agent: String,
    pub session: String,
    pub state: String,
    pub bg: String,
    /// `@agent_pid`: the agent the pane tracks.
    pub agent_pid: String,
}

impl After {
    /// The pane read before the event, with the event's writes applied.
    pub fn from(pane: &Pane, ops: &[Op]) -> After {
        let value = |name: &str, before: &str| -> String {
            ops.iter()
                .rev()
                .find_map(|op| match op {
                    Op::Set(n, v) if *n == name => Some(v.clone()),
                    Op::Unset(n) if *n == name => Some(String::new()),
                    _ => None,
                })
                .unwrap_or_else(|| before.to_string())
        };
        After {
            agent: value("@agent", &pane.agent),
            session: value("@agent_session", &pane.sid),
            state: value("@agent_state", &pane.state),
            bg: value("@agent_bg", &pane.bg),
            agent_pid: value("@agent_pid", &pane.agent_pid),
        }
    }
}

/// X5, the end of observation (`watch()`): after an observation, whether to
/// go on. The pane's session may change under it (it follows the new one);
/// the turn is over per rollout, or 10 s after a Stop or Interrupt that the
/// rollout never confirmed.
pub fn keep_watching(
    session: &mut String,
    after: &After,
    stored: Option<&CodexPane>,
    now_ms: u64,
    finished_since: &mut Option<u64>,
) -> bool {
    if after.session.is_empty() || after.agent != "codex" {
        return false;
    }
    if after.session != *session {
        *session = after.session.clone();
        *finished_since = None;
        return true;
    }
    let (turn_done, terminal) = match stored {
        Some(data) => {
            let r = &data.rollout;
            let done = r.turn.as_deref() == Some(data.turn.as_str())
                && matches!(r.status.as_str(), "complete" | "aborted" | "error");
            (done, data.terminal)
        }
        None => (false, false),
    };
    let mut over = turn_done;
    if terminal {
        let since = *finished_since.get_or_insert(now_ms);
        over = over || now_ms.saturating_sub(since) >= 10_000;
    } else {
        *finished_since = None;
    }
    !(after.state == "ready" || (over && after.bg.is_empty()))
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
mod tests;
