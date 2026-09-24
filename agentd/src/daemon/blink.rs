//! K1-K5, the turn signal, as agents/bin/agent-blink draws it: an amber band
//! sweeping across the names of sessions and windows that need you, a soft
//! green one at half speed over finished work not seen yet. The daemon draws
//! it with the same targets, texts and frames, and writes only what changed
//! from the previous frame.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use rustix::fs::{FlockOperation, flock};
use tokio::task::spawn_local;
use tokio::time::{sleep, timeout};

use super::tmux::Cmd;
use super::{Daemon, now_ms};
use crate::proto::Reply;

/// K4: 12 frames a cycle (6 sweeping, 3 lit, 3 dark); one every 70 ms while
/// something needs you, every 140 ms when only unseen work blinks.
const FRAMES: u64 = 12;
const FAST: Duration = Duration::from_millis(70);
const SLOW: Duration = Duration::from_millis(140);
/// K5: the targets are looked at again every 6 frames.
const REFRESH_EVERY: u64 = 6;
/// K1: `@agent_unseen_blink_for` when unset.
const UNSEEN_FOR: i64 = 120;
/// How often to try the lock while agent-blink holds it.
const LOCK_RETRY: Duration = Duration::from_secs(1);
/// Clearing on the way out must not hold the exit up.
const CLEAR_BUDGET: Duration = Duration::from_secs(1);

const RS: char = '\x1e';
const US: char = '\x1f';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Needs,
    Unseen,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Needs => "needs",
            Kind::Unseen => "unseen",
        }
    }
}

/// K4: how many characters are lit in frame `f` of a text of `len`.
fn lit(len: usize, f: u64) -> usize {
    match f {
        0..6 => (len * (f as usize + 1)).div_ceil(6),
        6..9 => len,
        _ => 0,
    }
}

/// One refresh's read: every pane, the globals, and each session's and
/// window's text as the bar shows it (K3) and whether it is lit.
fn refresh_read() -> Vec<Cmd> {
    let w = |s: &str| s.to_string();
    vec![
        vec![
            w("list-panes"),
            w("-a"),
            w("-F"),
            format!(
                "{RS}P{US}#{{session_name}}{US}#{{window_id}}{US}#{{@agent_state}}{US}#{{@agent_since}}{US}#{{@agent_subs}}{US}#{{@agent_bg}}"
            ),
        ],
        vec![
            w("display"),
            w("-p"),
            format!("{RS}G{US}#{{@agent_unseen_blink_for}}{US}#{{@blink-demo}}"),
        ],
        vec![
            w("list-sessions"),
            w("-F"),
            format!("{RS}S{US}#{{session_name}}{US}#{{=/16/…:session_name}}{US}#{{@blink-s}}"),
        ],
        vec![
            w("list-windows"),
            w("-a"),
            w("-F"),
            format!(
                "{RS}W{US}#{{window_id}}{US}#{{?@narrow-tabs,#{{=/14/…:#{{E:@agent-task}}}},#{{=/22/…:#{{E:@agent-task}}}}}}{US}#{{@blink-w}}"
            ),
        ],
    ]
}

/// A session or window that exists.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Label {
    text: String,
    /// `@blink-s`/`@blink-w` is on.
    on: bool,
}

/// What one refresh read.
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    /// (session, window, state, since, subs, bg) of every pane.
    panes: Vec<[String; 6]>,
    unseen_for: String,
    demo: String,
    sessions: BTreeMap<String, Label>,
    windows: BTreeMap<String, Label>,
}

fn parse(out: &str) -> Seen {
    let mut seen = Seen::default();
    for record in out.split(RS).skip(1) {
        let record = record.strip_suffix('\n').unwrap_or(record);
        let Some((tag, rest)) = record.split_once(US) else {
            continue;
        };
        let f: Vec<&str> = rest.split(US).collect();
        let label = |text: &str, on: &str| Label {
            text: text.to_string(),
            on: on == "1",
        };
        match (tag, f.as_slice()) {
            ("P", [a, b, c, d, e, g]) => seen.panes.push([a, b, c, d, e, g].map(|s| s.to_string())),
            ("G", [unseen, demo]) => {
                seen.unseen_for = unseen.to_string();
                seen.demo = demo.to_string();
            }
            ("S", [name, text, on]) => {
                seen.sessions.insert(name.to_string(), label(text, on));
            }
            // A window in several sessions: the first, like `display -t`.
            ("W", [id, text, on]) => {
                seen.windows
                    .entry(id.to_string())
                    .or_insert_with(|| label(text, on));
            }
            _ => {}
        }
    }
    seen
}

type Targets = BTreeMap<String, Kind>;

/// K1: the sessions and windows that blink, and whether the demo is over
/// (then it is unset).
fn targets(seen: &Seen, now: i64) -> (Targets, Targets, bool) {
    let unseen_for = seen.unseen_for.parse().unwrap_or(UNSEEN_FOR);
    let mut sessions = BTreeMap::new();
    let mut windows = BTreeMap::new();
    let num = |s: &str| s.parse::<i64>().unwrap_or(0);
    for [session, window, state, since, subs, bg] in &seen.panes {
        if session.starts_with("_peek-") {
            continue;
        }
        if state == "needs" {
            sessions.insert(session.clone(), Kind::Needs);
            windows.insert(window.clone(), Kind::Needs);
        } else if state == "done"
            && num(subs) < 1
            && num(bg) < 1
            && (unseen_for == 0 || now - num(since) < unseen_for)
        {
            sessions.entry(session.clone()).or_insert(Kind::Unseen);
            windows.entry(window.clone()).or_insert(Kind::Unseen);
        }
    }
    let mut demo_over = false;
    if let [session, window, until] = seen.demo.split_whitespace().collect::<Vec<_>>()[..] {
        if now < num(until) {
            sessions.insert(session.to_string(), Kind::Needs);
            windows.insert(window.to_string(), Kind::Needs);
        } else {
            demo_over = true;
        }
    }
    (sessions, windows, demo_over)
}

#[derive(Debug)]
struct Target {
    kind: Kind,
    text: String,
    /// `@blink-s`/`@blink-w` is written.
    on: bool,
    /// The lit length written last.
    shown: Option<usize>,
}

/// K2: a session target (`s`) or a window target (`w`), and its options.
#[derive(Debug, Clone, Copy)]
enum Scope {
    Session,
    Window,
}

impl Scope {
    fn option(self, suffix: &str) -> String {
        match self {
            Scope::Session => format!("@blink-s{suffix}"),
            Scope::Window => format!("@blink-w{suffix}"),
        }
    }

    /// `set` with its flags and `-t`: a session by its exact name, a window by id.
    fn command(self, target: &str, unset: bool) -> Cmd {
        let (flags, target) = match self {
            Scope::Session => (if unset { "-qu" } else { "" }, format!("={target}:")),
            Scope::Window => (if unset { "-wqu" } else { "-w" }, target.to_string()),
        };
        ["set", flags, "-t"]
            .into_iter()
            .filter(|w| !w.is_empty())
            .map(String::from)
            .chain([target])
            .collect()
    }

    fn set(self, target: &str, suffix: &str, value: &str) -> Cmd {
        let mut c = self.command(target, false);
        c.extend([self.option(suffix), value.to_string()]);
        c
    }

    /// Its four options go.
    fn clear(self, target: &str) -> Vec<Cmd> {
        ["", "-lit", "-rest", "-kind"]
            .into_iter()
            .map(|suffix| {
                let mut c = self.command(target, true);
                c.push(self.option(suffix));
                c
            })
            .collect()
    }
}

/// The animation between frames: what each target shows.
#[derive(Debug, Default)]
struct Animator {
    sessions: BTreeMap<String, Target>,
    windows: BTreeMap<String, Target>,
    any_needs: bool,
}

impl Animator {
    /// K1, K2: takes the targets of a refresh; what to write for them. False
    /// when nothing blinks any more.
    fn refresh(&mut self, seen: &Seen, now: i64) -> (Vec<Cmd>, bool) {
        let (want_s, want_w, demo_over) = targets(seen, now);
        let mut cmds = Vec::new();
        if demo_over {
            cmds.push(vec!["set".into(), "-gu".into(), "@blink-demo".into()]);
        }
        for (scope, current, want, exist) in [
            (Scope::Session, &mut self.sessions, &want_s, &seen.sessions),
            (Scope::Window, &mut self.windows, &want_w, &seen.windows),
        ] {
            for (t, label) in exist {
                // Stopped, or left lit by an animator that is gone (killed,
                // or a daemon before this one).
                if !want.contains_key(t) && (current.contains_key(t) || label.on) {
                    cmds.extend(scope.clear(t));
                }
            }
            // Gone targets are just forgotten: unsetting them would fail, and
            // tmux skips the commands after a failing one.
            current.retain(|t, _| want.contains_key(t));
            for (t, kind) in want {
                let Some(label) = exist.get(t) else {
                    continue;
                };
                let target = current.entry(t.clone()).or_insert_with(|| Target {
                    kind: *kind,
                    text: label.text.clone(),
                    on: false,
                    shown: None,
                });
                if target.kind != *kind || !target.on {
                    target.kind = *kind;
                    cmds.push(scope.set(t, "-kind", kind.as_str()));
                }
                if target.text != label.text {
                    target.text = label.text.clone();
                    target.shown = None;
                }
            }
        }
        self.any_needs = self
            .sessions
            .values()
            .chain(self.windows.values())
            .any(|t| t.kind == Kind::Needs);
        (cmds, !self.sessions.is_empty() || !self.windows.is_empty())
    }

    /// K4: the writes of frame `frame`, only where they change something.
    fn frame(&mut self, frame: u64) -> Vec<Cmd> {
        // Green goes at half speed: a frame every other tick while amber sets the pace.
        let slow = (if self.any_needs { frame / 2 } else { frame }) % FRAMES;
        let mut cmds = Vec::new();
        for (scope, targets) in [
            (Scope::Session, &mut self.sessions),
            (Scope::Window, &mut self.windows),
        ] {
            for (t, target) in targets.iter_mut() {
                let f = match target.kind {
                    Kind::Needs => frame % FRAMES,
                    Kind::Unseen => slow,
                };
                let k = lit(target.text.chars().count(), f);
                if !target.on {
                    cmds.push(scope.set(t, "", "1"));
                    target.on = true;
                }
                if target.shown != Some(k) {
                    let at = target
                        .text
                        .char_indices()
                        .nth(k)
                        .map_or(target.text.len(), |(i, _)| i);
                    cmds.push(scope.set(t, "-lit", &target.text[..at]));
                    cmds.push(scope.set(t, "-rest", &target.text[at..]));
                    target.shown = Some(k);
                }
            }
        }
        cmds
    }

    fn period(&self) -> Duration {
        if self.any_needs { FAST } else { SLOW }
    }

    /// Everything the targets show goes, target by target.
    fn clear(&mut self) -> Vec<Vec<Cmd>> {
        let sessions = std::mem::take(&mut self.sessions)
            .into_keys()
            .map(|t| Scope::Session.clear(&t));
        let windows = std::mem::take(&mut self.windows)
            .into_keys()
            .map(|t| Scope::Window.clear(&t));
        sessions.chain(windows).collect()
    }

    fn targets(&self) -> BTreeSet<String> {
        self.sessions
            .keys()
            .chain(self.windows.keys())
            .cloned()
            .collect()
    }
}

/// The daemon's animator: at most one per tmux server, agent-blink's
/// included (they share its lock), and only while something blinks (K5).
pub struct Blink {
    lock: PathBuf,
    /// It runs, or waits for the lock.
    running: Cell<bool>,
    /// Asked to blink while running: look at the targets at the next frame.
    stale: Cell<bool>,
    /// The daemon is going: no more frames.
    stopped: Cell<bool>,
    animator: RefCell<Animator>,
}

impl Blink {
    /// agent-blink's lock, `blink-<socket name>.lock` in the runtime directory.
    pub fn new(runtime_dir: &Path, socket: &Path) -> Blink {
        let name = socket.file_name().unwrap_or_default().to_string_lossy();
        Blink {
            lock: runtime_dir.join(format!("blink-{name}.lock")),
            running: Cell::new(false),
            stale: Cell::new(false),
            stopped: Cell::new(false),
            animator: RefCell::default(),
        }
    }

    fn try_lock(&self) -> Option<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock)
            .ok()?;
        flock(&file, FlockOperation::NonBlockingLockExclusive).ok()?;
        Some(file)
    }

    /// What blinks now, for `ctl status`.
    pub fn targets(&self) -> BTreeSet<String> {
        self.animator.borrow().targets()
    }
}

impl Daemon {
    /// K5: whatever needs it blinks: starts the animator, or has the running
    /// one look at the targets at its next frame.
    pub(super) fn blink(self: &Rc<Self>) {
        let b = &self.blink;
        if b.stopped.get() {
            return;
        }
        b.stale.set(true);
        if !b.running.replace(true) {
            spawn_local(self.clone().animate());
        }
    }

    async fn animate(self: Rc<Self>) {
        let b = &self.blink;
        // agent-blink may be the one drawing: it stops when nothing blinks,
        // and then this one looks.
        let lock = loop {
            if b.stopped.get() {
                b.running.set(false);
                return;
            }
            match b.try_lock() {
                Some(lock) => break lock,
                None => sleep(LOCK_RETRY).await,
            }
        };
        let mut frame = 0;
        while !b.stopped.get() {
            if frame % REFRESH_EVERY == 0 || b.stale.get() {
                b.stale.set(false);
                let Ok(out) = self.tmux.run(&refresh_read()).await else {
                    break;
                };
                let (cmds, blinking) = b.animator.borrow_mut().refresh(&parse(&out), now_secs());
                if !cmds.is_empty() && self.tmux.run(&cmds).await.is_err() {
                    // Something went away meanwhile and tmux skipped the rest: start afresh.
                    *b.animator.borrow_mut() = Animator::default();
                    b.stale.set(true);
                }
                // Asked again meanwhile, maybe for something this read missed.
                if !blinking && !b.stale.get() {
                    break;
                }
            }
            let cmds = b.animator.borrow_mut().frame(frame);
            if !cmds.is_empty() && self.tmux.run(&cmds).await.is_err() {
                *b.animator.borrow_mut() = Animator::default();
                b.stale.set(true);
            }
            frame += 1;
            let period = b.animator.borrow().period();
            sleep(period).await;
        }
        b.running.set(false);
        drop(lock);
    }

    /// K1: `ctl blink-demo <session> <window id> [seconds]`, prefix+Q's
    /// preview: they blink as if they needed you, for a while (10 s).
    pub(super) async fn blink_demo(self: &Rc<Self>, args: &[String]) -> Reply {
        let usage = || Reply::error("usage: agentd ctl blink-demo <session> <window id> [seconds]");
        let (session, window, seconds) = match args {
            [session, window] => (session, window, 10),
            [session, window, seconds] => match seconds.parse::<i64>() {
                Ok(seconds) => (session, window, seconds),
                Err(_) => return usage(),
            },
            _ => return usage(),
        };
        let demo = format!("{session} {window} {}", now_secs() + seconds);
        let set = vec!["set".into(), "-g".into(), "@blink-demo".into(), demo];
        if self.tmux.run(&[set]).await.is_err() {
            return Reply::error("tmux did not take @blink-demo");
        }
        self.blink();
        Reply::ok()
    }

    /// On the way out: nothing left lit that no animator would clear.
    pub(super) async fn stop_blink(&self) {
        self.blink.stopped.set(true);
        let targets = self.blink.animator.borrow_mut().clear();
        let _ = timeout(CLEAR_BUDGET, async {
            for cmds in targets {
                let _ = self.tmux.run(&cmds).await;
            }
        })
        .await;
    }
}

fn now_secs() -> i64 {
    (now_ms() / 1000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(session: &str, window: &str, state: &str, since: i64) -> [String; 6] {
        [session, window, state, &since.to_string(), "", ""].map(String::from)
    }

    fn label(text: &str) -> Label {
        Label {
            text: text.into(),
            on: false,
        }
    }

    /// Every pane's session and window exist; a window's text is its id's.
    fn seen(panes: Vec<[String; 6]>) -> Seen {
        let mut s = Seen {
            panes,
            ..Seen::default()
        };
        for p in s.panes.clone() {
            s.sessions.insert(p[0].clone(), label(&p[0]));
            s.windows
                .insert(p[1].clone(), label(&format!("task of {}", p[1])));
        }
        s
    }

    /// The value a frame wrote to `option` of `target`.
    fn written(cmds: &[Cmd], target: &str, option: &str) -> Option<String> {
        cmds.iter()
            .find(|c| {
                c[c.len() - 2] == option
                    && c.iter().any(|w| w == target || *w == format!("={target}:"))
            })
            .map(|c| c[c.len() - 1].clone())
    }

    #[test]
    fn k4_lit_lengths() {
        let lits: Vec<usize> = (0..12).map(|f| lit(17, f)).collect();
        assert_eq!(lits, [3, 6, 9, 12, 15, 17, 17, 17, 17, 0, 0, 0]);
        assert_eq!(lit(0, 3), 0);
    }

    #[test]
    fn k1_targets() {
        let now = 1000;
        let mut s = seen(vec![
            pane("a", "@1", "needs", 0),
            pane("a", "@2", "done", now - 10),
            pane("b", "@3", "done", now - 10),
            pane("c", "@4", "done", now - 500), // unseen for too long
            pane("_peek-1", "@5", "needs", 0),
        ]);
        let busy = |session: &str, window: &str, subs: &str, bg: &str| {
            [session, window, "done", &(now - 5).to_string(), subs, bg].map(String::from)
        };
        s.panes.push(busy("d", "@6", "2", ""));
        s.panes.push(busy("e", "@7", "", "1"));
        let (sessions, windows, _) = targets(&s, now);
        assert_eq!(
            sessions,
            BTreeMap::from([("a".into(), Kind::Needs), ("b".into(), Kind::Unseen)])
        );
        assert_eq!(
            windows,
            BTreeMap::from([
                ("@1".into(), Kind::Needs),
                ("@2".into(), Kind::Unseen),
                ("@3".into(), Kind::Unseen)
            ])
        );
        s.unseen_for = "0".into(); // no limit
        assert!(targets(&s, now).0.contains_key("c"));
    }

    #[test]
    fn k1_demo() {
        let mut s = seen(vec![]);
        s.demo = "main @9 1010".into();
        let (sessions, windows, over) = targets(&s, 1000);
        assert_eq!(
            (sessions["main"], windows["@9"], over),
            (Kind::Needs, Kind::Needs, false)
        );
        assert!(targets(&s, 1010).2, "over: to be unset");
        let mut a = Animator::default();
        s.sessions.insert("main".into(), label("main"));
        let (cmds, blinking) = a.refresh(&s, 1010);
        assert!(!blinking);
        assert_eq!(
            cmds,
            [vec!["set".to_string(), "-gu".into(), "@blink-demo".into()]]
        );
    }

    #[test]
    fn k2_k4_writes_only_what_changes() {
        let s = seen(vec![pane("main", "@1", "needs", 0)]);
        let mut a = Animator::default();
        let (cmds, blinking) = a.refresh(&s, 1000);
        assert!(blinking);
        assert_eq!(
            cmds,
            [
                Scope::Session.set("main", "-kind", "needs"),
                Scope::Window.set("@1", "-kind", "needs")
            ]
        );
        let first = a.frame(0);
        assert_eq!(first.len(), 6, "on, lit and rest of both");
        assert_eq!(written(&first, "main", "@blink-s").as_deref(), Some("1"));
        assert_eq!(
            written(&first, "main", "@blink-s-lit").as_deref(),
            Some("m")
        );
        assert_eq!(
            written(&first, "main", "@blink-s-rest").as_deref(),
            Some("ain")
        );
        assert_eq!(written(&first, "@1", "@blink-w-lit").as_deref(), Some("ta"));
        // Lit and rest where the lit length changes: "main" stays at 2 in
        // frame 2, both are whole from 5 to 8 and dark from 9 to 11.
        let lengths: Vec<usize> = (1..12).map(|f| a.frame(f).len()).collect();
        assert_eq!(lengths, [4, 2, 4, 4, 2, 0, 0, 0, 4, 0, 0]);
        // The same targets at the next refresh: nothing to write.
        assert_eq!(a.refresh(&s, 1000), (vec![], true));
        assert!(a.frame(12).len() == 4);
    }

    #[test]
    fn k2_stopped_targets_lose_their_options() {
        let mut a = Animator::default();
        a.refresh(&seen(vec![pane("main", "@1", "needs", 0)]), 1000);
        a.frame(0);
        let (cmds, blinking) = a.refresh(&seen(vec![pane("main", "@1", "idle", 0)]), 1000);
        assert!(!blinking);
        let mut expected = Scope::Session.clear("main");
        expected.extend(Scope::Window.clear("@1"));
        assert_eq!(cmds, expected);
        assert_eq!(
            expected[..4],
            [
                vec![
                    "set".to_string(),
                    "-qu".into(),
                    "-t".into(),
                    "=main:".into(),
                    "@blink-s".into()
                ],
                vec![
                    "set".to_string(),
                    "-qu".into(),
                    "-t".into(),
                    "=main:".into(),
                    "@blink-s-lit".into()
                ],
                vec![
                    "set".to_string(),
                    "-qu".into(),
                    "-t".into(),
                    "=main:".into(),
                    "@blink-s-rest".into()
                ],
                vec![
                    "set".to_string(),
                    "-qu".into(),
                    "-t".into(),
                    "=main:".into(),
                    "@blink-s-kind".into()
                ],
            ]
        );
    }

    #[test]
    fn k2_gone_targets_are_forgotten_not_unset() {
        let mut a = Animator::default();
        a.refresh(&seen(vec![pane("main", "@1", "needs", 0)]), 1000);
        // The session was killed: unsetting its options would fail the rest.
        assert_eq!(a.refresh(&Seen::default(), 1000), (vec![], false));
    }

    #[test]
    fn k2_leftovers_of_another_animator_go() {
        let mut s = seen(vec![pane("main", "@1", "needs", 0)]);
        s.sessions.insert(
            "old".into(),
            Label {
                text: "old".into(),
                on: true,
            },
        );
        s.windows.get_mut("@1").unwrap().on = true;
        let (cmds, _) = Animator::default().refresh(&s, 1000);
        let mut expected = Scope::Session.clear("old");
        expected.push(Scope::Session.set("main", "-kind", "needs"));
        expected.push(Scope::Window.set("@1", "-kind", "needs"));
        assert_eq!(
            cmds, expected,
            "a target still wanted is taken over, not cleared"
        );
    }

    #[test]
    fn k4_unseen_next_to_needs_takes_every_other_frame() {
        let mut a = Animator::default();
        a.refresh(
            &seen(vec![
                pane("n", "@1", "needs", 0),
                pane("uuuuuu", "@2", "done", 995),
            ]),
            1000,
        );
        assert_eq!(a.period(), Duration::from_millis(70));
        let unseen: Vec<Option<String>> = (0..8)
            .map(|f| written(&a.frame(f), "uuuuuu", "@blink-s-lit"))
            .collect();
        let expected = [
            Some("u"),
            None,
            Some("uu"),
            None,
            Some("uuu"),
            None,
            Some("uuuu"),
            None,
        ];
        assert_eq!(unseen, expected.map(|l| l.map(String::from)));
        let mut only_unseen = Animator::default();
        only_unseen.refresh(&seen(vec![pane("u", "@2", "done", 995)]), 1000);
        assert_eq!(only_unseen.period(), Duration::from_millis(140));
    }

    #[test]
    fn k3_texts_by_characters() {
        let mut a = Animator::default();
        let mut s = seen(vec![pane("main", "@1", "needs", 0)]);
        s.windows.insert("@1".into(), label("✳ Refactor…"));
        a.refresh(&s, 1000);
        let cmds = a.frame(2);
        let (shown, rest) = (
            written(&cmds, "@1", "@blink-w-lit").unwrap(),
            written(&cmds, "@1", "@blink-w-rest").unwrap(),
        );
        assert_eq!(format!("{shown}{rest}"), "✳ Refactor…");
        assert_eq!(shown.chars().count(), lit(11, 2));
        // A new text (a new task) is written whole at the next frame.
        s.windows.insert("@1".into(), label("Other"));
        a.refresh(&s, 1000);
        assert_eq!(
            written(&a.frame(3), "@1", "@blink-w-rest").as_deref(),
            Some("r")
        );
    }

    #[test]
    fn k5_clear_on_the_way_out() {
        let mut a = Animator::default();
        a.refresh(&seen(vec![pane("main", "@1", "needs", 0)]), 1000);
        assert_eq!(
            a.clear(),
            [Scope::Session.clear("main"), Scope::Window.clear("@1")]
        );
        assert!(a.targets().is_empty());
    }

    #[test]
    fn parse_refresh() {
        let out = format!(
            "{RS}P{US}main{US}@1{US}needs{US}5{US}{US}\n{RS}G{US}30{US}main @1 99\n{RS}S{US}main{US}main{US}1\n{RS}W{US}@1{US}Fix…{US}\n{RS}W{US}@1{US}other session's view{US}\n"
        );
        let s = parse(&out);
        assert_eq!(
            s.panes,
            vec![["main", "@1", "needs", "5", "", ""].map(String::from)]
        );
        assert_eq!(
            (s.unseen_for.as_str(), s.demo.as_str()),
            ("30", "main @1 99")
        );
        assert_eq!(
            s.sessions["main"],
            Label {
                text: "main".into(),
                on: true
            }
        );
        assert_eq!(
            s.windows["@1"],
            label("Fix…"),
            "a window in two sessions: the first"
        );
    }
}
