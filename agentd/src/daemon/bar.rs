//! L1: the plain values the top row reads when agentd runs, so that the row
//! loops over no pane (tmux 3.6 keeps memory on every redraw of a row whose
//! formats loop over every pane with nested `E:`). Per session:
//! `@s-glyphs` (its agents' glyphs, as `#{W:#{P:#{E:@agent-glyph}}}`
//! draws them), `@s-unseen` (1 while one of them is ✓ and not seen) and
//! `@s-other-<state>` (how many agents of the other sessions of its space
//! are in that state: the summary, as the `@cnt-*` formats of agent-spaces
//! count them). agent-spaces builds the row from them when `@agentd` is set.

use std::collections::BTreeMap;

use super::control;
use super::tmux::Cmd;

/// The summary's states, in its order.
pub const STATES: [&str; 5] = ["needs", "done", "working", "idle", "untracked"];

const RS: char = '\x1e';
const US: char = '\x1f';

/// What the row needs of a pane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Pane {
    session: String,
    agent: String,
    state: String,
    subs: String,
    bg: String,
    command: String,
}

/// A session and the values it has now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Session {
    name: String,
    space: String,
    values: Values,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Values {
    glyphs: String,
    unseen: String,
    other: [String; 5],
}

/// One read: every pane in the order of `#{W:#{P:}}`, and every session
/// with its space and its values.
pub fn read() -> Vec<Cmd> {
    let pane = [
        "#{session_name}",
        "#{@agent}",
        "#{@agent_state}",
        "#{@agent_subs}",
        "#{@agent_bg}",
        "#{pane_current_command}",
    ]
    .join(&US.to_string());
    let mut session = vec![
        "#{session_name}".to_string(),
        "#{?@space,#{@space},#{@space_auto}}".to_string(),
        "#{@s-glyphs}".to_string(),
        "#{@s-unseen}".to_string(),
    ];
    session.extend(STATES.iter().map(|s| format!("#{{@s-other-{s}}}")));
    vec![
        vec![
            "list-panes".into(),
            "-a".into(),
            "-F".into(),
            format!("{RS}P{US}{pane}"),
        ],
        vec![
            "list-sessions".into(),
            "-F".into(),
            format!("{RS}S{US}{}", session.join(&US.to_string())),
        ],
    ]
}

fn parse(out: &str) -> (Vec<Pane>, Vec<Session>) {
    let (mut panes, mut sessions) = (Vec::new(), Vec::new());
    for record in out.split(RS).skip(1) {
        let record = record.strip_suffix('\n').unwrap_or(record);
        let f: Vec<&str> = record.split(US).collect();
        match f.as_slice() {
            ["P", session, agent, state, subs, bg, command] => panes.push(Pane {
                session: session.to_string(),
                agent: agent.to_string(),
                state: state.to_string(),
                subs: subs.to_string(),
                bg: bg.to_string(),
                command: command.to_string(),
            }),
            ["S", name, space, glyphs, unseen, other @ ..] if other.len() == 5 => {
                sessions.push(Session {
                    name: name.to_string(),
                    space: space.to_string(),
                    values: Values {
                        glyphs: glyphs.to_string(),
                        unseen: unseen.to_string(),
                        other: std::array::from_fn(|i| other[i].to_string()),
                    },
                })
            }
            _ => {}
        }
    }
    (panes, sessions)
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

    fn untracked(&self) -> bool {
        self.agent.is_empty() && matches!(self.command.as_str(), "claude" | "codex")
    }

    /// `@agent-glyph`, as agents.conf defines it.
    fn glyph(&self) -> String {
        let state = self.state.as_str();
        let base = match state {
            "needs" => "#[fg=#{@ac-needs}]▲",
            "error" => "#[fg=#{@ac-error}]✗",
            "working" => "#[fg=#{@ac-working}]●",
            "compacting" => "#[fg=#{@ac-compact}]↻",
            _ if self.background() => "#[fg=#{@ac-working}]◐",
            "done" => "#[fg=#{@ac-done}]✓",
            // `#{@agent}` is true unless empty or 0.
            _ if !self.agent.is_empty() && self.agent != "0" => "#[fg=#{@ac-idle}]○",
            _ if self.untracked() => "#[fg=#{@ac-dim}]◇",
            _ => "",
        };
        let mut glyph = base.to_string();
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
            self.untracked(),
        ]
        .map(u32::from)
    }
}

fn peek(session: &str) -> bool {
    session.starts_with("_peek-")
}

/// The values every session should have.
fn wanted(panes: &[Pane], sessions: &[Session]) -> BTreeMap<String, Values> {
    let mut glyphs: BTreeMap<&str, String> = BTreeMap::new();
    let mut unseen: BTreeMap<&str, bool> = BTreeMap::new();
    let mut counts: BTreeMap<&str, [u32; 5]> = BTreeMap::new();
    for p in panes {
        glyphs.entry(&p.session).or_default().push_str(&p.glyph());
        *unseen.entry(&p.session).or_default() |= p.unseen();
        let c = counts.entry(&p.session).or_default();
        for (total, n) in c.iter_mut().zip(p.counts()) {
            *total += n;
        }
    }
    let mut out = BTreeMap::new();
    for s in sessions {
        if s.name == control::SESSION {
            continue;
        }
        // The other sessions of its space (all of them in no space), never
        // the peek ones: what `@cnt-<state>-<space>` counts for its row.
        let mut other = [0u32; 5];
        for t in sessions {
            if t.name == s.name || peek(&t.name) || !(s.space.is_empty() || t.space == s.space) {
                continue;
            }
            if let Some(c) = counts.get(t.name.as_str()) {
                for (total, n) in other.iter_mut().zip(c) {
                    *total += n;
                }
            }
        }
        out.insert(
            s.name.clone(),
            Values {
                glyphs: glyphs.get(s.name.as_str()).cloned().unwrap_or_default(),
                unseen: if unseen.get(s.name.as_str()).copied().unwrap_or(false) {
                    "1".into()
                } else {
                    String::new()
                },
                other: other.map(|n| n.to_string()),
            },
        );
    }
    out
}

/// What to write, one request per session (a session gone meanwhile fails
/// its request only).
pub fn update(out: &str) -> Vec<Vec<Cmd>> {
    let (panes, sessions) = parse(out);
    let wanted = wanted(&panes, &sessions);
    let mut requests = Vec::new();
    for s in &sessions {
        let Some(want) = wanted.get(&s.name) else {
            continue;
        };
        let target = format!("={}:", s.name);
        let set = |option: &str, value: &str| -> Cmd {
            if value.is_empty() {
                vec![
                    "set".into(),
                    "-qu".into(),
                    "-t".into(),
                    target.clone(),
                    option.into(),
                ]
            } else {
                vec![
                    "set".into(),
                    "-t".into(),
                    target.clone(),
                    option.into(),
                    value.into(),
                ]
            }
        };
        let mut cmds = Vec::new();
        if s.values.glyphs != want.glyphs {
            cmds.push(set("@s-glyphs", &want.glyphs));
        }
        if s.values.unseen != want.unseen {
            cmds.push(set("@s-unseen", &want.unseen));
        }
        for (i, state) in STATES.iter().enumerate() {
            if s.values.other[i] != want.other[i] {
                cmds.push(set(&format!("@s-other-{state}"), &want.other[i]));
            }
        }
        if !cmds.is_empty() {
            requests.push(cmds);
        }
    }
    requests
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(session: &str, agent: &str, state: &str) -> Pane {
        Pane {
            session: session.into(),
            agent: agent.into(),
            state: state.into(),
            ..Pane::default()
        }
    }

    fn session(name: &str, space: &str) -> Session {
        Session {
            name: name.into(),
            space: space.into(),
            ..Session::default()
        }
    }

    #[test]
    fn l1_glyphs_as_agent_glyph() {
        let g = |p: Pane| p.glyph();
        assert_eq!(g(pane("s", "claude", "needs")), "#[fg=#{@ac-needs}]▲");
        assert_eq!(g(pane("s", "claude", "error")), "#[fg=#{@ac-error}]✗");
        assert_eq!(g(pane("s", "codex", "working")), "#[fg=#{@ac-working}]●");
        assert_eq!(
            g(pane("s", "claude", "compacting")),
            "#[fg=#{@ac-compact}]↻"
        );
        assert_eq!(g(pane("s", "claude", "done")), "#[fg=#{@ac-done}]✓");
        assert_eq!(g(pane("s", "claude", "idle")), "#[fg=#{@ac-idle}]○");
        assert_eq!(g(pane("s", "claude", "")), "#[fg=#{@ac-idle}]○");
        // Background work wins over done and idle, not over needs or working.
        let mut bg = pane("s", "claude", "done");
        bg.bg = "2".into();
        assert_eq!(g(bg.clone()), "#[fg=#{@ac-working}]◐");
        bg.state = "needs".into();
        assert_eq!(g(bg), "#[fg=#{@ac-needs}]▲");
        // Subagents add their count.
        let mut subs = pane("s", "claude", "working");
        subs.subs = "2".into();
        assert_eq!(g(subs), "#[fg=#{@ac-working}]●#[fg=#{@ac-dim}]+2");
        // An agent without hooks, and a plain shell.
        let mut untracked = pane("s", "", "");
        untracked.command = "codex".into();
        assert_eq!(g(untracked), "#[fg=#{@ac-dim}]◇");
        assert_eq!(g(pane("s", "", "")), "");
    }

    #[test]
    fn l1_counts_as_the_summary() {
        let c = |p: Pane| p.counts();
        assert_eq!(c(pane("s", "claude", "needs")), [1, 0, 0, 0, 0]);
        assert_eq!(c(pane("s", "claude", "done")), [0, 1, 0, 0, 0]);
        assert_eq!(c(pane("s", "claude", "compacting")), [0, 0, 1, 0, 0]);
        assert_eq!(c(pane("s", "claude", "ready")), [0, 0, 0, 1, 0]);
        assert_eq!(c(pane("s", "claude", "error")), [0, 0, 0, 0, 0]);
        let mut bg = pane("s", "claude", "needs");
        bg.subs = "1".into();
        assert_eq!(
            c(bg),
            [1, 0, 1, 0, 0],
            "needs, and working in the background"
        );
        let mut untracked = pane("s", "", "");
        untracked.command = "claude".into();
        assert_eq!(c(untracked), [0, 0, 0, 0, 1]);
    }

    #[test]
    fn l1_values_per_session() {
        let mut working = pane("api", "claude", "working");
        working.subs = "1".into();
        let panes = vec![
            pane("main", "claude", "needs"),
            pane("main", "claude", "done"),
            working,
            pane("docs", "codex", "done"),
            pane("zulu", "claude", "needs"),
            pane("_peek-7", "claude", "needs"),
        ];
        let sessions = vec![
            session("main", "work"),
            session("api", "work"),
            session("docs", "work"),
            session("zulu", "home"),
            session("free", ""),
            session("_peek-7", "work"),
            session("_peek-agentd", ""),
        ];
        let w = wanted(&panes, &sessions);
        assert_eq!(w["main"].glyphs, "#[fg=#{@ac-needs}]▲#[fg=#{@ac-done}]✓");
        assert_eq!(w["main"].unseen, "1");
        assert_eq!(w["api"].unseen, "", "working");
        // main's row: api and docs (its space), never itself, zulu or the peek.
        assert_eq!(w["main"].other, ["0", "1", "1", "0", "0"].map(String::from));
        assert_eq!(w["api"].other, ["1", "2", "0", "0", "0"].map(String::from));
        // In no space: every other session but peeks.
        assert_eq!(w["free"].other, ["2", "2", "1", "0", "0"].map(String::from));
        // A peek session shows its space, all of it.
        assert_eq!(
            w["_peek-7"].other,
            ["1", "2", "1", "0", "0"].map(String::from)
        );
        assert!(!w.contains_key("_peek-agentd"));
    }

    #[test]
    fn l1_writes_only_what_changed() {
        let out = format!(
            "{RS}P{US}main{US}claude{US}needs{US}{US}{US}claude\n{RS}P{US}api{US}claude{US}idle{US}{US}{US}claude\n\
             {RS}S{US}api{US}work{US}#[fg=#{{@ac-idle}}]○{US}{US}1{US}0{US}0{US}0{US}0\n\
             {RS}S{US}main{US}work{US}old{US}1{US}0{US}0{US}0{US}0{US}0\n"
        );
        let requests = update(&out);
        let set = |t: &str, o: &str, v: &str| -> Cmd {
            ["set", "-t", t, o, v].map(String::from).to_vec()
        };
        assert_eq!(
            requests,
            [vec![
                set("=main:", "@s-glyphs", "#[fg=#{@ac-needs}]▲"),
                vec![
                    "set".into(),
                    "-qu".into(),
                    "-t".into(),
                    "=main:".into(),
                    "@s-unseen".into()
                ],
                set("=main:", "@s-other-idle", "1"),
            ]]
        );
    }
}
