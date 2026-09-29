//! L1: the plain values the top row reads when agentd runs, so that drawing
//! it loops over no pane and reads no /proc. The row is redrawn at every
//! blink frame, option write and title change, and counting the agents there
//! looped over every pane dozens of times per redraw.
//!
//! Per session: `@s-glyphs` (its agents' glyphs, as
//! `#{W:#{P:#{E:@agent-glyph}}}` draws them), `@s-unseen` (1 while one of
//! them is ✓ and not seen) and `@s-other-<state>` (how many agents of the
//! other sessions of its space are in that state: its summary, as the
//! `@cnt-*` formats of agent-spaces count it; unset for none). Per pane,
//! `@p-untracked` (1 for a `claude` or `codex` that reports nothing, the ◇),
//! which `@agent-untracked` in agents.conf reads instead of
//! `pane_current_command`. agent-spaces builds the row from them when
//! `@agentd` is set.
//!
//! One read (`display -p`) gives everything they come from and their values
//! now; only what differs is written, in one request (each write redraws
//! every client). It runs when something they come from may have changed:
//! our own writes that change what the row shows (`Bar` remembers what the
//! last read found: every event writes its state again), a window or a
//! session added or closed (our control client hears it), a pane closed
//! (`agentd bar` from agents.conf's hooks), sessions tagged by agent-spaces
//! (`ctl bar`). Nothing is polled: a control-mode subscription would do it,
//! but tmux checks one every second and reading every pane's command costs it
//! ~1.2 ms each time at 40 panes. So a `claude` started without hooks in a
//! pane that stays shows its ◇ at the next of those.

use std::collections::{HashMap, HashSet};

use super::control;
use super::tmux::Cmd;
use crate::core::Op;

/// The summary's states, in its order (agent-spaces' SUMMARY_STATES).
const STATES: [&str; 5] = ["needs", "done", "working", "idle", "untracked"];

/// The session's space, as everywhere in agents/.
const SPACE: &str = "#{?@space,#{@space},#{@space_auto}}";
/// 1 for a pane without `@agent` running `claude` or `codex`: tmux reads
/// /proc for the command, so only there, as `@agent-glyph` does.
const UNTRACKED: &str = "#{?@agent,0,#{m/r:^(claude|codex)$,#{pane_current_command}}}";

const RS: char = '\x1e';
const US: char = '\x1f';

/// The pane options the row shows, in the order `Bar` keeps them.
const SHOWN: [&str; 4] = ["@agent", "@agent_state", "@agent_subs", "@agent_bg"];

/// What the last read found of each pane's options the row shows: every
/// event writes its pane's state again, and writing what is already there
/// needs no read.
#[derive(Default)]
pub struct Bar {
    known: HashMap<String, [String; 4]>,
}

impl Bar {
    /// Whether writing `ops` to `pane` may change what the row shows.
    pub fn changes(&self, pane: &str, ops: &[Op]) -> bool {
        let known = self.known.get(pane);
        ops.iter().any(|op| {
            let (name, value) = match op {
                Op::Set(name, value) => (*name, value.as_str()),
                Op::Unset(name) => (*name, ""),
            };
            SHOWN
                .iter()
                .position(|s| *s == name)
                .is_some_and(|i| known.is_none_or(|k| k[i] != value))
        })
    }

    /// The writes a read calls for; what it found is remembered.
    pub fn update(&mut self, out: &str) -> Vec<Cmd> {
        let sessions = parse(out);
        self.known = sessions
            .iter()
            .flat_map(|s| &s.panes)
            .map(|p| {
                let shown = [&p.agent, &p.state, &p.subs, &p.bg].map(String::clone);
                (p.id.clone(), shown)
            })
            .collect();
        writes(&sessions)
    }
}

/// Every session (its id, whether it is ours or a peek, its space and its
/// values), each followed by its panes in the order of `#{W:#{P:}}`. The
/// command is read only for panes without `@agent`, as `@agent-glyph` does.
fn format() -> String {
    let own = control::SESSION;
    let mut session = vec![
        "#{session_id}".to_string(),
        format!(
            "#{{?#{{==:#{{session_name}},{own}}},own,#{{?#{{m:_peek-*,#{{session_name}}}},peek,}}}}"
        ),
        SPACE.to_string(),
        "#{@s-glyphs}".to_string(),
        "#{@s-unseen}".to_string(),
    ];
    session.extend(STATES.iter().map(|s| format!("#{{@s-other-{s}}}")));
    let pane = [
        "#{pane_id}",
        "#{@agent}",
        "#{@agent_state}",
        "#{@agent_subs}",
        "#{@agent_bg}",
        UNTRACKED,
        "#{@p-untracked}",
    ];
    format!(
        "#{{S:{RS}S{US}{}#{{W:#{{P:{RS}P{US}{}}}}}}}",
        session.join(&US.to_string()),
        pane.join(&US.to_string())
    )
}

/// One expansion of it (the context doesn't matter: `#{S:}` is every session).
pub fn read() -> Cmd {
    vec!["display".into(), "-p".into(), format()]
}

/// What the row needs of a pane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Pane {
    id: String,
    agent: String,
    state: String,
    subs: String,
    bg: String,
    /// A `claude` or `codex` in a pane without `@agent`.
    untracked: bool,
    /// `@p-untracked` now.
    shown_untracked: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Session {
    id: String,
    /// `own` (agentd's), `peek`, or empty.
    kind: String,
    space: String,
    panes: Vec<Pane>,
    /// The values it has now.
    shown: Values,
}

/// A session's values, empty for unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Values {
    glyphs: String,
    unseen: String,
    other: [String; 5],
}

fn parse(out: &str) -> Vec<Session> {
    let mut sessions: Vec<Session> = Vec::new();
    for record in out.split(RS).skip(1) {
        let record = record.strip_suffix('\n').unwrap_or(record);
        let f: Vec<&str> = record.split(US).collect();
        match f.as_slice() {
            ["S", id, kind, space, glyphs, unseen, other @ ..] if other.len() == STATES.len() => {
                sessions.push(Session {
                    id: id.to_string(),
                    kind: kind.to_string(),
                    space: space.to_string(),
                    panes: Vec::new(),
                    shown: Values {
                        glyphs: glyphs.to_string(),
                        unseen: unseen.to_string(),
                        other: std::array::from_fn(|i| other[i].to_string()),
                    },
                })
            }
            ["P", id, agent, state, subs, bg, untracked, shown] => {
                if let Some(s) = sessions.last_mut() {
                    s.panes.push(Pane {
                        id: id.to_string(),
                        agent: agent.to_string(),
                        state: state.to_string(),
                        subs: subs.to_string(),
                        bg: bg.to_string(),
                        untracked: *untracked == "1",
                        shown_untracked: shown.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    sessions
}

/// `#{e|>:<v>,0}`.
fn positive(v: &str) -> bool {
    v.trim().parse::<f64>().is_ok_and(|n| n > 0.0)
}

impl Pane {
    /// `@agent-background`.
    fn background(&self) -> bool {
        positive(&self.subs) || positive(&self.bg)
    }

    /// `@agent-glyph`, as agents.conf defines it.
    fn glyph(&self) -> String {
        let mut glyph = match self.state.as_str() {
            "needs" => "#[fg=#{@ac-needs}]▲".to_string(),
            "error" => "#[fg=#{@ac-error}]✗".to_string(),
            "working" => "#[fg=#{@ac-working}]●".to_string(),
            "compacting" => "#[fg=#{@ac-compact}]↻".to_string(),
            // ◐ for subagents, else ○, with a dakuten (U+3099) for shells.
            _ if self.background() => {
                let circle = if positive(&self.subs) { "◐" } else { "○" };
                let shells = if positive(&self.bg) { "\u{3099}" } else { "" };
                format!("#[fg=#{{@ac-working}}]{circle}{shells}")
            }
            "done" => "#[fg=#{@ac-done}]✓".to_string(),
            // `#{@agent}` is true unless empty or 0.
            _ if !self.agent.is_empty() && self.agent != "0" => "#[fg=#{@ac-idle}]○".to_string(),
            _ if self.untracked => "#[fg=#{@ac-dim}]◇".to_string(),
            _ => String::new(),
        };
        if positive(&self.subs) {
            glyph.push_str(&format!("#[fg=#{{@ac-dim}}]+{}", self.subs));
        }
        glyph
    }

    /// `@agent-unseen`.
    fn unseen(&self) -> bool {
        self.state == "done" && !self.background()
    }

    /// Which of the summary's states it counts in (agent-spaces' C_* formats):
    /// one with background work counts as working too.
    fn counts(&self) -> [u32; 5] {
        let state = self.state.as_str();
        let background = self.background();
        [
            state == "needs",
            state == "done" && !background,
            matches!(state, "working" | "compacting") || background,
            matches!(state, "idle" | "ready") && !background,
            self.untracked && self.agent.is_empty(),
        ]
        .map(u32::from)
    }
}

impl Session {
    fn counts(&self) -> [u32; 5] {
        let mut total = [0; 5];
        for p in &self.panes {
            for (t, n) in total.iter_mut().zip(p.counts()) {
                *t += n;
            }
        }
        total
    }
}

/// The values a session should have. Peek sessions show no chips, only
/// their summary.
fn wanted(s: &Session, sessions: &[Session], counts: &[[u32; 5]]) -> Values {
    // The other sessions of its space (all of them in no space), never the
    // peek ones: what `@cnt-<state>-<space>` counts for its row.
    let mut other = [0u32; 5];
    for (t, c) in sessions.iter().zip(counts) {
        if t.id == s.id || !t.kind.is_empty() || !(s.space.is_empty() || t.space == s.space) {
            continue;
        }
        for (total, n) in other.iter_mut().zip(c) {
            *total += n;
        }
    }
    let user = s.kind.is_empty();
    Values {
        glyphs: if user {
            s.panes.iter().map(Pane::glyph).collect()
        } else {
            String::new()
        },
        unseen: if user && s.panes.iter().any(Pane::unseen) {
            "1".into()
        } else {
            String::new()
        },
        other: other.map(|n| if n == 0 { String::new() } else { n.to_string() }),
    }
}

/// What to write after a read: every value that differs from what it
/// found, as one request.
fn writes(sessions: &[Session]) -> Vec<Cmd> {
    let counts: Vec<[u32; 5]> = sessions.iter().map(Session::counts).collect();
    let mut cmds = Vec::new();
    let set = |cmds: &mut Vec<Cmd>, scope: &str, target: &str, option: &str, value: &str| {
        let mut c: Cmd = vec!["set".into()];
        if value.is_empty() {
            c.push(format!("{scope}u"));
        } else if scope != "-" {
            c.push(scope.into());
        }
        c.extend(["-t".into(), target.into(), option.into()]);
        if !value.is_empty() {
            c.push(value.into());
        }
        cmds.push(c);
    };
    let mut panes_done = HashSet::new();
    for s in sessions {
        if s.kind == "own" {
            continue;
        }
        let want = wanted(s, sessions, &counts);
        if s.shown.glyphs != want.glyphs {
            set(&mut cmds, "-", &s.id, "@s-glyphs", &want.glyphs);
        }
        if s.shown.unseen != want.unseen {
            set(&mut cmds, "-", &s.id, "@s-unseen", &want.unseen);
        }
        for (i, state) in STATES.iter().enumerate() {
            if s.shown.other[i] != want.other[i] {
                let option = format!("@s-other-{state}");
                set(&mut cmds, "-", &s.id, &option, &want.other[i]);
            }
        }
        // A window in several sessions (a group) has its panes in each.
        for p in &s.panes {
            let want = if p.untracked { "1" } else { "" };
            if p.shown_untracked != want && panes_done.insert(p.id.as_str()) {
                set(&mut cmds, "-p", &p.id, "@p-untracked", want);
            }
        }
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(agent: &str, state: &str) -> Pane {
        Pane {
            agent: agent.into(),
            state: state.into(),
            ..Pane::default()
        }
    }

    fn session(id: &str, kind: &str, space: &str, panes: Vec<Pane>) -> Session {
        Session {
            id: id.into(),
            kind: kind.into(),
            space: space.into(),
            panes,
            shown: Values::default(),
        }
    }

    fn untracked() -> Pane {
        Pane {
            untracked: true,
            ..Pane::default()
        }
    }

    #[test]
    fn l1_glyphs_as_agent_glyph() {
        let g = |p: Pane| p.glyph();
        assert_eq!(g(pane("claude", "needs")), "#[fg=#{@ac-needs}]▲");
        assert_eq!(g(pane("claude", "error")), "#[fg=#{@ac-error}]✗");
        assert_eq!(g(pane("codex", "working")), "#[fg=#{@ac-working}]●");
        assert_eq!(g(pane("claude", "compacting")), "#[fg=#{@ac-compact}]↻");
        assert_eq!(g(pane("claude", "done")), "#[fg=#{@ac-done}]✓");
        assert_eq!(g(pane("claude", "idle")), "#[fg=#{@ac-idle}]○");
        assert_eq!(g(pane("claude", "")), "#[fg=#{@ac-idle}]○");
        // Background work wins over done and idle, not over needs or working.
        let mut bg = pane("claude", "done");
        bg.bg = "2".into();
        assert_eq!(g(bg.clone()), "#[fg=#{@ac-working}]○\u{3099}");
        // ◐ is for subagents; the dakuten, for shells.
        bg.subs = "1".into();
        assert_eq!(
            g(bg.clone()),
            "#[fg=#{@ac-working}]◐\u{3099}#[fg=#{@ac-dim}]+1"
        );
        bg.bg = "0".into();
        assert_eq!(g(bg.clone()), "#[fg=#{@ac-working}]◐#[fg=#{@ac-dim}]+1");
        bg.state = "needs".into();
        assert_eq!(g(bg), "#[fg=#{@ac-needs}]▲#[fg=#{@ac-dim}]+1");
        // Subagents add their count.
        let mut subs = pane("claude", "working");
        subs.subs = "2".into();
        assert_eq!(g(subs), "#[fg=#{@ac-working}]●#[fg=#{@ac-dim}]+2");
        // An agent without hooks, and a plain shell.
        assert_eq!(g(untracked()), "#[fg=#{@ac-dim}]◇");
        assert_eq!(g(pane("", "")), "");
    }

    #[test]
    fn l1_counts_as_the_summary() {
        let c = |p: Pane| p.counts();
        assert_eq!(c(pane("claude", "needs")), [1, 0, 0, 0, 0]);
        assert_eq!(c(pane("claude", "done")), [0, 1, 0, 0, 0]);
        assert_eq!(c(pane("claude", "compacting")), [0, 0, 1, 0, 0]);
        assert_eq!(c(pane("claude", "ready")), [0, 0, 0, 1, 0]);
        assert_eq!(c(pane("claude", "error")), [0, 0, 0, 0, 0]);
        let mut bg = pane("claude", "needs");
        bg.subs = "1".into();
        assert_eq!(
            c(bg),
            [1, 0, 1, 0, 0],
            "needs, and working in the background"
        );
        let mut done_bg = pane("claude", "done");
        done_bg.bg = "1".into();
        assert_eq!(c(done_bg), [0, 0, 1, 0, 0], "done but still at work");
        assert_eq!(c(untracked()), [0, 0, 0, 0, 1]);
        assert_eq!(c(pane("", "")), [0; 5]);
    }

    #[test]
    fn l1_values_per_session() {
        let mut working = pane("claude", "working");
        working.subs = "1".into();
        let sessions = vec![
            session(
                "$0",
                "",
                "work",
                vec![pane("claude", "needs"), pane("claude", "done")],
            ),
            session("$1", "", "work", vec![working, pane("", "")]),
            session("$2", "", "work", vec![pane("codex", "done"), untracked()]),
            session("$3", "", "home", vec![pane("claude", "needs")]),
            session("$4", "", "", vec![pane("", "")]),
            session("$5", "peek", "work", vec![pane("claude", "needs")]),
            session("$6", "own", "", vec![pane("", "")]),
        ];
        let counts: Vec<[u32; 5]> = sessions.iter().map(Session::counts).collect();
        let w = |i: usize| wanted(&sessions[i], &sessions, &counts);
        let n = |a: [&str; 5]| a.map(String::from);
        assert_eq!(w(0).glyphs, "#[fg=#{@ac-needs}]▲#[fg=#{@ac-done}]✓");
        assert_eq!(w(0).unseen, "1");
        assert_eq!(w(1).unseen, "", "working");
        assert_eq!(w(2).glyphs, "#[fg=#{@ac-done}]✓#[fg=#{@ac-dim}]◇");
        // $0's row: $1 and $2 (its space), never itself, $3 or the peek.
        assert_eq!(w(0).other, n(["", "1", "1", "", "1"]));
        assert_eq!(w(1).other, n(["1", "2", "", "", "1"]));
        // In no space: every other session but peeks.
        assert_eq!(w(4).other, n(["2", "2", "1", "", "1"]));
        // A peek session shows its space, all of it, and no chips.
        assert_eq!(w(5).other, n(["1", "2", "1", "", "1"]));
        assert_eq!((w(5).glyphs.as_str(), w(5).unseen.as_str()), ("", ""));
    }

    fn record(tag: &str, fields: &[&str]) -> String {
        format!("{RS}{tag}{US}{}", fields.join(&US.to_string()))
    }

    #[test]
    fn l1_writes_only_what_changed() {
        let out = [
            record("S", &["$0", "", "work", "old", "1", "", "", "", "", ""]),
            record("P", &["%0", "claude", "needs", "", "", "0", ""]),
            record("P", &["%1", "", "", "", "", "1", ""]),
            record(
                "S",
                &[
                    "$1",
                    "",
                    "work",
                    "#[fg=#{@ac-idle}]○",
                    "",
                    "",
                    "",
                    "",
                    "1",
                    "1",
                ],
            ),
            record("P", &["%2", "claude", "idle", "", "", "0", ""]),
            record("P", &["%3", "", "", "", "", "0", "1"]),
            // A group: the same panes again, written once.
            record("S", &["$2", "peek", "work", "", "", "", "", "1", "", ""]),
            record("P", &["%2", "claude", "idle", "", "", "0", ""]),
            record("P", &["%3", "", "", "", "", "0", "1"]),
            // Ours: nothing, whatever it has.
            record("S", &["$3", "own", "", "x", "", "", "", "", "", ""]),
            record("P", &["%4", "", "", "", "", "0", ""]),
        ]
        .concat()
            + "\n";
        let c = |w: &[&str]| -> Cmd { w.iter().map(|s| s.to_string()).collect() };
        let mut bar = Bar::default();
        assert_eq!(
            bar.update(&out),
            [
                c(&[
                    "set",
                    "-t",
                    "$0",
                    "@s-glyphs",
                    "#[fg=#{@ac-needs}]▲#[fg=#{@ac-dim}]◇"
                ]),
                c(&["set", "-u", "-t", "$0", "@s-unseen"]),
                c(&["set", "-t", "$0", "@s-other-idle", "1"]),
                c(&["set", "-p", "-t", "%1", "@p-untracked", "1"]),
                c(&["set", "-t", "$1", "@s-other-needs", "1"]),
                c(&["set", "-u", "-t", "$1", "@s-other-idle"]),
                c(&["set", "-pu", "-t", "%3", "@p-untracked"]),
                c(&["set", "-t", "$2", "@s-other-needs", "1"]),
                c(&["set", "-u", "-t", "$2", "@s-other-working"]),
                c(&["set", "-t", "$2", "@s-other-idle", "1"]),
                c(&["set", "-t", "$2", "@s-other-untracked", "1"]),
            ]
        );
        // Written, nothing is left to write.
        let settled = [
            record(
                "S",
                &[
                    "$0",
                    "",
                    "work",
                    "#[fg=#{@ac-idle}]○",
                    "",
                    "",
                    "",
                    "",
                    "1",
                    "",
                ],
            ),
            record("P", &["%0", "claude", "idle", "", "", "0", ""]),
            record(
                "S",
                &[
                    "$1",
                    "",
                    "work",
                    "#[fg=#{@ac-idle}]○",
                    "",
                    "",
                    "",
                    "",
                    "1",
                    "",
                ],
            ),
            record("P", &["%2", "claude", "idle", "", "", "0", ""]),
        ]
        .concat();
        assert!(bar.update(&settled).is_empty());
        assert!(bar.update("").is_empty());
    }

    #[test]
    fn l1_writing_what_is_there_reads_nothing() {
        let mut bar = Bar::default();
        let set = |name: &'static str, value: &str| Op::Set(name, value.into());
        // Before any read, every write of what the row shows counts.
        assert!(bar.changes("%0", &[set("@agent_state", "working")]));
        let out = [
            record("S", &["$0", "", "", "", "", "", "", "", "", ""]),
            record("P", &["%0", "claude", "working", "", "", "0", ""]),
        ]
        .concat();
        bar.update(&out);
        // The state every event writes again.
        let again = [set("@agent", "claude"), set("@agent_state", "working")];
        assert!(!bar.changes("%0", &again));
        assert!(!bar.changes("%0", &[set("@agent_tool", "Bash: ls")]));
        assert!(!bar.changes("%0", &[Op::Unset("@agent_bg")]));
        assert!(bar.changes("%0", &[set("@agent_state", "needs")]));
        assert!(bar.changes("%0", &[set("@agent_subs", "1")]));
        assert!(bar.changes("%0", &[Op::Unset("@agent")]));
        assert!(bar.changes("%1", &again), "a pane the read did not see");
    }

    #[test]
    fn l1_one_read_for_every_session_and_pane() {
        let read = &read()[2];
        assert!(read.starts_with("#{S:") && read.contains("#{W:#{P:"));
        // Commands are read only where `@agent-glyph` reads them.
        assert!(read.contains("#{?@agent,0,#{m/r:^(claude|codex)$,#{pane_current_command}}}"));
    }
}
