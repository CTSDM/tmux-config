//! The pure core (design.md, "Inside the daemon"): an event and the facts read
//! for it go in, option writes and effects come out. No I/O, no clock: time,
//! visibility and everything read from tmux or /proc arrive in [`Facts`].
//! Rule ids in comments and test names are those of docs/daemon/contract.md.

pub mod codex;
mod events;
pub mod reconcile;
mod text;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub use agentd_common::event::{Event, Kind, line};
pub use agentd_common::ownership::{owner, remote_owner};
pub use codex::CodexFacts;
pub use text::{escape_hashes, notify_title, subtypes};

/// One hook call that passed ownership (I4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub kind: Kind,
    pub pane: String,
    pub event: Event,
    /// `CLAUDE_CONFIG_DIR` of the agent, if set (H14).
    pub config_dir: Option<String>,
    /// The agent process found by the ownership walk (or, observing, the
    /// one the pane tracks).
    pub agent_pid: u32,
    /// A Codex observation made by the daemon, not a hook (X5).
    pub observing: bool,
    /// I6: an event sent again for a new pane, already lived through: it
    /// joins no round.
    pub replay: bool,
}

/// The pane as tmux has it when the event is handled (P2), plus what
/// visibility, titles and mute need. Options are raw (`##` as stored).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pane {
    pub state: String,
    pub since: String,
    pub prev: String,
    pub needs_id: String,
    /// `@agent_needs`.
    pub needs: String,
    pub tool: String,
    pub tests_sound_at: String,
    /// `@agent_session`: the sid the pane had before this event.
    pub sid: String,
    pub session: String,
    pub title: String,
    /// `@space`, else `@space_auto`, of the pane's session.
    pub space: String,
    /// `@agent_mute_<space>` is `on`.
    pub muted: bool,
    /// `@agent`, `@agent_turn`, `@agent_transcript`, `@agent_pid`,
    /// `@agent_pid_start`, `@agent_bg`, `@agent_bg_watch`.
    pub agent: String,
    pub turn: String,
    pub transcript: String,
    pub agent_pid: String,
    pub agent_pid_start: String,
    pub bg: String,
    pub bg_watch: String,
}

/// Another pane of the server, for rounds (R).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherPane {
    pub pane: String,
    pub state: String,
    pub subs: String,
    /// `@agent_session`: which subagent sets are still live (sweep).
    pub sid: String,
    pub space: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Visible,
    Session,
    Away,
}

/// Everything read for one event. `vis` is only computed when
/// [`may_need_visibility`] says so, `others` when [`may_need_panes`] does.
#[derive(Debug, Clone)]
pub struct Facts {
    /// Codex events only.
    pub codex: Option<CodexFacts>,
    pub now: i64,
    pub host: String,
    pub pane: Pane,
    pub vis: Option<Visibility>,
    pub others: Vec<OtherPane>,
    /// Shells the agent runs in the background (B1), counted at Stop.
    pub bg_shells: u32,
    /// Global `@agent_remind_after`, raw.
    pub remind_after: String,
    /// Global `@agent_test_regex`, raw.
    pub test_regex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Set(&'static str, String),
    Unset(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        }
    }
}

/// What an event makes happen, in order (O1): the pane's option writes
/// first, then the rest.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Options(Vec<Op>),
    /// Close the pane's notification (N3).
    NotifyClose,
    Sound(&'static str),
    /// Keep `@agent_bg` up to date for the agent's background shells (B1);
    /// the Stop that found them has written it already.
    Bgwatch {
        agent_pid: u32,
    },
    /// The reminder (N4): fire after `after` seconds if the pane is still in
    /// the `needs` that started at `since`. Replaces the previous one (C1).
    RemindArm {
        after: f64,
        since: i64,
    },
    RemindCancel,
    Notify {
        urgency: Urgency,
        title: String,
        body: String,
    },
    /// Make sure the turn signal runs (K5).
    Blink,
    /// Observe this Codex pane until its turn is over (X5).
    Watch,
    /// H5b: watch for the answer to the permission dialog of the `needs`
    /// that started at `since` (the Bash tool's shell starting).
    AwaitAnswer {
        since: i64,
    },
}

/// The daemon's own bookkeeping (P2: never seeded from options).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Running subagents per agent session: sid -> agent id -> type (A1-A3).
    pub subagents: BTreeMap<String, BTreeMap<String, String>>,
    /// Rounds per space: the panes that took part (R).
    pub rounds: BTreeMap<String, BTreeSet<String>>,
    /// Codex bookkeeping per pane (§6).
    pub codex: BTreeMap<String, codex::CodexPane>,
}

/// Whether handling `ev` may need the pane's visibility (V1: entering
/// `needs`, `done` or `error`). A superset: false means it never does.
pub fn may_need_visibility(kind: Kind, ev: &str) -> bool {
    match kind {
        Kind::Claude => matches!(
            ev,
            "PermissionRequest"
                | "Notification"
                | "Elicitation"
                | "PostCompact"
                | "Stop"
                | "StopFailure"
        ),
        // A Codex wait can show up on almost any event (X3).
        Kind::Codex => true,
    }
}

/// Whether handling `ev` may need the other panes of the server (R).
pub fn may_need_panes(kind: Kind, ev: &str) -> bool {
    match kind {
        Kind::Claude => ev == "Stop",
        // An observation can end the turn as a Stop (X5).
        Kind::Codex => matches!(ev, "Stop" | "CodexReconcile"),
    }
}

/// Whether handling `ev` may count Codex command trees (X7).
pub fn may_need_procs(kind: Kind, ev: &str) -> bool {
    kind == Kind::Codex && matches!(ev, "Stop" | "Interrupt" | "CodexReconcile")
}

/// Whether handling `ev` may need the agent's background shells (B1).
pub fn may_need_bg_shells(kind: Kind, ev: &str) -> bool {
    kind == Kind::Claude && ev == "Stop"
}

/// Handles one hook event (or Codex observation).
pub fn handle(state: &mut State, input: &Input, facts: &Facts) -> Vec<Effect> {
    events::handle(state, input, facts)
}

/// E1, seen: the pane got focus. A finished pane is seen (`done` → `idle`,
/// `@agent_since` untouched) and its notification closes.
pub fn seen(pane: &Pane) -> Vec<Effect> {
    let ops = if pane.state == "done" {
        vec![Op::Set("@agent_state", "idle".into())]
    } else {
        Vec::new()
    };
    vec![Effect::Options(ops), Effect::NotifyClose]
}

/// The reminder fired (N4, C1, C2): what to do now that `@agent_remind_after`
/// has passed since the `needs` that started at `since`.
pub fn reminder(since: i64, facts: &Facts) -> Vec<Effect> {
    let pane = &facts.pane;
    if pane.state != "needs" || pane.since != since.to_string() || pane.muted {
        return Vec::new();
    }
    if facts.vis == Some(Visibility::Visible) {
        return Vec::new();
    }
    let minutes = (facts.now - since).div_euclid(60);
    vec![
        Effect::Sound("come-to-papa"),
        Effect::Notify {
            urgency: Urgency::Critical,
            title: notify_title(&pane.session, &pane.title, &facts.host),
            body: format!("Still waiting for you, {minutes} min now"),
        },
    ]
}

/// H5b: a shell of Claude's Bash tool started while the pane waited for a
/// permission since `since`: you answered Yes, and the command runs. As H5:
/// `working`, the wait's notification closes and its reminder goes (N3, C1).
pub fn answered(since: i64, facts: &Facts) -> Vec<Effect> {
    let pane = &facts.pane;
    if pane.state != "needs" || pane.needs != "permission" || pane.since != since.to_string() {
        return Vec::new();
    }
    vec![
        Effect::Options(vec![
            Op::Set("@agent_since", facts.now.to_string()),
            Op::Set("@agent_state", "working".into()),
            Op::Unset("@agent_needs"),
            Op::Unset("@agent_needs_id"),
        ]),
        Effect::NotifyClose,
        Effect::RemindCancel,
    ]
}

/// Every option of the contract's table (P1), as `AG_OPTS` in agent-lib.sh.
pub const ALL_OPTIONS: [&str; 25] = [
    "@agent",
    "@agent_session",
    "@agent_profile",
    "@agent_model",
    "@agent_mode",
    "@agent_state",
    "@agent_needs",
    "@agent_needs_id",
    "@agent_since",
    "@agent_prev",
    "@agent_tool",
    "@agent_msg",
    "@agent_subs",
    "@agent_subtypes",
    "@agent_transcript",
    "@agent_tests_sound_at",
    "@agent_bg",
    "@agent_bg_watch",
    "@agent_turn",
    "@agent_outcome",
    "@agent_question",
    "@agent_collaboration",
    "@agent_pid",
    "@agent_pid_start",
    "@agent_codex_watch",
];

#[cfg(test)]
mod tests;
