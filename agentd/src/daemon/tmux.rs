//! The tmux transport of phases 1-3 (design.md, "tmux transport"): one
//! spawned `tmux` for everything an event reads, one for what it writes.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

use super::bar;
use super::control::{self, Control};
use crate::core::{Op, OtherPane, Pane};

/// Record and field separators in tmux output: unlike tabs or newlines, no
/// name, title or option value we read has them.
const RS: char = '\x1e';
const US: char = '\x1f';

/// A wedged tmux server must not hold a pane's events forever.
const TIMEOUT: Duration = Duration::from_secs(3);
/// Attaching again after our control client went away.
const REATTACH: Duration = Duration::from_secs(1);
/// A daemon started while tmux loads its configuration is there before the
/// user's first session: its own is alone for a moment.
const FIRST_SESSION_GRACE: Duration = Duration::from_secs(5);

/// The global option naming our control client, for agents.conf (F1).
const AGENTD_CLIENT: &str = "@agentd_client";

/// The session's space, as everywhere in agents/.
const SPACE: &str = "#{?@space,#{@space},#{@space_auto}}";

/// `@agent_mute_<space>`, the space sanitized as in `ag_space_muted`: the
/// option name is built by substitution and expanded a second time.
const MUTE: &str = "#{E:#{s/XSPACEX/#{s/[^A-Za-z0-9_-]/_/:#{?@space,#{@space},#{@space_auto}}}/:#{l:#{@agent_mute_XSPACEX}}}}";

/// The pane's fields, title last (it is the most likely to hold odd bytes).
const PANE_FIELDS: [&str; 28] = [
    "#{pane_id}",
    "#{pane_pid}",
    "#{@agent_state}",
    "#{@agent_since}",
    "#{@agent_prev}",
    "#{@agent_needs_id}",
    "#{@agent_tool}",
    "#{@agent_tests_sound_at}",
    "#{@agent_session}",
    "#{session_name}",
    "#{window_active}",
    "#{pane_active}",
    SPACE,
    MUTE,
    "#{@agent_remind_after}",
    "#{@agent_test_regex}",
    "#{@agents_bin}",
    "#{@agent}",
    "#{@agent_turn}",
    "#{@agent_transcript}",
    "#{@agent_pid}",
    "#{@agent_pid_start}",
    "#{@agent_bg}",
    "#{@agent_bg_watch}",
    "#{@agent_sound}",
    "#{@agent_sound_volume}",
    "#{@agent_needs}",
    "#{pane_title}",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub name: String,
    pub pid: u32,
    pub flags: String,
    pub session: String,
    pub control: bool,
}

/// Everything read for one event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Read {
    pub pane_pid: u32,
    pub pane: Pane,
    pub window_active: bool,
    pub pane_active: bool,
    pub remind_after: String,
    pub test_regex: String,
    /// `@agents_bin`, where the bash helpers are.
    pub bin: String,
    /// `@agent_sound` and `@agent_sound_volume` (S9).
    pub sound: String,
    pub sound_volume: String,
    pub clients: Vec<Client>,
    pub others: Vec<OtherPane>,
}

/// Why a read found nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// tmux answered that the pane does not exist.
    Pane,
    /// tmux failed otherwise, or did not answer in time.
    Tmux,
}

pub struct Tmux {
    socket: PathBuf,
    /// `$TMUX` for the helpers, which run plain `tmux`.
    env: String,
    /// Phase 4: our control-mode client; `None` until attached, or when
    /// `AGENTD_TRANSPORT=spawn`.
    control: RefCell<Option<Control>>,
    use_control: bool,
    last_attach: Cell<Option<Instant>>,
    /// A session was created or closed (`%sessions-changed`), or we attached.
    sessions_changed: Rc<Notify>,
    /// Z1: our session was all that was left and we closed it; no attaching
    /// until the user has a session again.
    closed: Cell<bool>,
    /// A session of the user's was there at some point.
    seen_user: Cell<bool>,
    /// Messages to our client (`display-message -c`), for the daemon.
    messages: mpsc::UnboundedSender<String>,
    to_daemon: RefCell<Option<mpsc::UnboundedReceiver<String>>>,
    /// L1: what the top row shows may have changed (bar.rs), and what the
    /// last read found.
    bar_changed: Rc<Notify>,
    bar: RefCell<bar::Bar>,
}

/// One tmux command, word by word.
pub type Cmd = Vec<String>;

fn cmd(words: &[&str]) -> Cmd {
    words.iter().map(|w| w.to_string()).collect()
}

impl Tmux {
    pub fn new(socket: PathBuf, server_pid: u32) -> Self {
        let env = format!("{},{server_pid},0", socket.display());
        let use_control = std::env::var("AGENTD_TRANSPORT").as_deref() != Ok("spawn");
        let (messages, to_daemon) = mpsc::unbounded_channel();
        Tmux {
            bar_changed: Rc::new(Notify::new()),
            bar: RefCell::default(),
            socket,
            env,
            control: RefCell::new(None),
            use_control,
            last_attach: Cell::new(None),
            sessions_changed: Rc::new(Notify::new()),
            closed: Cell::new(false),
            seen_user: Cell::new(false),
            messages,
            to_daemon: RefCell::new(Some(to_daemon)),
        }
    }

    /// The messages to our client, once.
    pub fn messages(&self) -> Option<mpsc::UnboundedReceiver<String>> {
        self.to_daemon.borrow_mut().take()
    }

    pub fn sessions_changed(&self) -> Rc<Notify> {
        self.sessions_changed.clone()
    }

    pub fn bar_changed(&self) -> Rc<Notify> {
        self.bar_changed.clone()
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    /// Which transport answers now.
    pub fn transport(&self) -> &'static str {
        match self.control.borrow().as_ref() {
            Some(c) if c.alive() => "control",
            _ => "spawn",
        }
    }

    /// Our control client, attached (again) when needed: after `%exit`
    /// while the server lives, at most once a second.
    pub fn attach(&self) {
        if !self.use_control
            || self.closed.get()
            || self.control.borrow().as_ref().is_some_and(Control::alive)
        {
            return;
        }
        if self
            .last_attach
            .get()
            .is_some_and(|t| t.elapsed() < REATTACH)
        {
            return;
        }
        self.last_attach.set(Some(Instant::now()));
        match Control::start(
            &self.socket,
            self.sessions_changed.clone(),
            self.bar_changed.clone(),
            self.messages.clone(),
        ) {
            Ok(control) => {
                // F1: hooks tell us things with a message to this client,
                // which makes the tmux server start no process.
                let name = [cmd(&["set", "-gF", AGENTD_CLIENT, "#{client_name}"])];
                if let Some(line) = control::line(&name) {
                    let _ = control.send(line, 1);
                }
                *self.control.borrow_mut() = Some(control);
                // Maybe ours is all there is (the last user session closed meanwhile).
                self.sessions_changed.notify_one();
            }
            Err(e) => super::log(&format!("control mode: {e}; spawning tmux")),
        }
    }

    /// Z1: when our session is all that is left, it goes and we stay away,
    /// so the server exits (`exit-empty`) as it would without us. Before the
    /// user's first session, it waits a little: the time to check again.
    pub async fn close_if_alone(&self) -> Option<Duration> {
        if self.closed.get() || !self.use_control {
            return None;
        }
        let list = [cmd(&["list-sessions", "-F", "#{session_name}"])];
        let sessions = self.run(&list).await.ok()?;
        if sessions.lines().any(|s| s != control::SESSION) {
            self.seen_user.set(true);
            return None;
        }
        if !self.seen_user.get() {
            let waited = self
                .last_attach
                .get()
                .map_or(Duration::MAX, |t| t.elapsed());
            if let Some(left) = FIRST_SESSION_GRACE
                .checked_sub(waited)
                .filter(|d| !d.is_zero())
            {
                return Some(left);
            }
        }
        super::log("our session is the last one: closing it");
        self.closed.set(true);
        // Our client goes, and destroy-unattached takes the session with it;
        // unless someone else is in it.
        self.control.borrow_mut().take();
        let kill = [
            cmd(&["set", "-gu", AGENTD_CLIENT]),
            cmd(&["kill-session", "-t", &format!("={}:", control::SESSION)]),
        ];
        let _ = self.spawn(&kill).await;
        None
    }

    /// The daemon goes: no hook should message a client that is gone.
    pub async fn forget_client(&self) {
        let _ = self.run(&[cmd(&["set", "-gu", AGENTD_CLIENT])]).await;
    }

    /// After closing: attach again once the user has a session (a server
    /// kept by `exit-empty off`, and a new session later). Never while the
    /// server is going away: then no session is left.
    async fn reopen(&self) {
        if self
            .last_attach
            .get()
            .is_some_and(|t| t.elapsed() < REATTACH)
        {
            return;
        }
        self.last_attach.set(Some(Instant::now()));
        let list = [cmd(&["list-sessions", "-F", "#{session_name}"])];
        if let Ok(sessions) = self.spawn(&list).await
            && sessions.lines().any(|s| s != control::SESSION)
        {
            super::log("the user has a session again: attaching");
            self.closed.set(false);
            self.last_attach.set(None);
        }
    }

    /// Runs the commands, in order, as one request; the output of them all.
    /// Through the control client when attached, else a spawned `tmux`.
    pub async fn run(&self, commands: &[Cmd]) -> Result<String, Missing> {
        if self.closed.get() {
            self.reopen().await;
        }
        self.attach();
        let answer = {
            let control = self.control.borrow();
            match (control.as_ref(), control::line(commands)) {
                (Some(c), Some(line)) => c.send(line, commands.len()),
                _ => None,
            }
        };
        let Some(answer) = answer else {
            return self.spawn(commands).await;
        };
        match timeout(TIMEOUT, answer).await {
            Ok(Ok(Ok(out))) => Ok(out),
            Ok(Ok(Err(error))) => {
                super::log(&format!("control mode: {}", error.trim()));
                Err(missing(&error))
            }
            // The client went away while we waited: this once, spawn.
            Ok(Err(_)) => self.spawn(commands).await,
            Err(_) => {
                // No answer: the stream can't be trusted; attach again later.
                if let Some(c) = self.control.borrow().as_ref() {
                    c.kill();
                }
                super::log("control mode: no answer, attaching again");
                Err(Missing::Tmux)
            }
        }
    }

    /// `tmux -S <socket> <commands>`, joined by `;` arguments.
    async fn spawn(&self, commands: &[Cmd]) -> Result<String, Missing> {
        let mut args: Vec<Cow<str>> = Vec::new();
        for (i, c) in commands.iter().enumerate() {
            if i > 0 {
                args.push(";".into());
            }
            args.extend(c.iter().map(|w| protect_semicolon(w)));
        }
        let child = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args.iter().map(|a| a.as_ref()))
            .env("TMUX", &self.env)
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();
        let out = match timeout(TIMEOUT, child).await {
            Ok(Ok(out)) => out,
            _ => return Err(Missing::Tmux),
        };
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(missing(&String::from_utf8_lossy(&out.stderr)))
        }
    }

    /// One request for the pane (P2 options, V2 facts, space, mute, §13
    /// globals), and when asked the clients (V1) and every pane (R).
    pub async fn read(&self, pane: &str, clients: bool, panes: bool) -> Result<Read, Missing> {
        let mut commands = vec![cmd(&[
            "display",
            "-p",
            "-t",
            pane,
            &format!("{RS}D{US}{}", PANE_FIELDS.join(&US.to_string())),
        ])];
        if clients {
            commands.push(cmd(&[
                "list-clients",
                "-F",
                &format!("{RS}C{US}#{{client_name}}{US}#{{client_pid}}{US}#{{client_flags}}{US}#{{client_session}}{US}#{{client_control_mode}}"),
            ]));
        }
        if panes {
            commands.push(cmd(&[
                "list-panes",
                "-a",
                "-F",
                &format!(
                    "{RS}P{US}#{{pane_id}}{US}#{{@agent_state}}{US}#{{@agent_subs}}{US}#{{@agent_session}}{US}{SPACE}"
                ),
            ]));
        }
        let out = self.run(&commands).await?;
        parse(&out).inspect_err(|_| {
            let head: String = out.chars().take(120).collect();
            super::log(&format!("read {pane}: unreadable answer {head:?}"));
        })
    }

    /// The pane's option writes, in order, as one request.
    pub async fn write(&self, pane: &str, ops: &[Op]) -> Result<(), Missing> {
        if ops.is_empty() {
            return Ok(());
        }
        let commands: Vec<Cmd> = ops
            .iter()
            .map(|op| match op {
                Op::Set(name, value) => cmd(&["set", "-p", "-t", pane, name, value]),
                Op::Unset(name) => cmd(&["set", "-pu", "-t", pane, name]),
            })
            .collect();
        let written = self.run(&commands).await.map(drop);
        if self.bar.borrow().changes(pane, ops) {
            self.bar_changed.notify_one();
        }
        written
    }

    /// L1: reads what the top row's values come from, and writes those
    /// that differ.
    pub async fn refresh_bar(&self) {
        let Ok(out) = self.run(&[bar::read()]).await else {
            return;
        };
        let writes = self.bar.borrow_mut().update(&out);
        if !writes.is_empty() {
            let _ = self.run(&writes).await;
        }
    }
}

/// What a failed command's message says about the pane.
fn missing(error: &str) -> Missing {
    if error.contains("no such pane") || error.contains("can't find pane") {
        Missing::Pane
    } else {
        Missing::Tmux
    }
}

/// tmux reads an argument ending in `;` as the end of a command and drops the
/// `;`; one ending in `\;` keeps a plain `;`. Bash stores `done` for a reply
/// `done;`; this keeps it.
fn protect_semicolon(value: &str) -> Cow<'_, str> {
    match value.strip_suffix(';') {
        Some(head) => format!("{head}\\;").into(),
        None => value.into(),
    }
}

/// A pane that is gone still "displays", with every field empty.
fn parse(out: &str) -> Result<Read, Missing> {
    let mut read = None;
    let mut clients = Vec::new();
    let mut others = Vec::new();
    for record in out.split(RS).skip(1) {
        let record = record.strip_suffix('\n').unwrap_or(record);
        let Some((tag, rest)) = record.split_once(US) else {
            continue;
        };
        match tag {
            "D" => {
                let f: Vec<&str> = rest.splitn(PANE_FIELDS.len(), US).collect();
                if f.len() != PANE_FIELDS.len() {
                    return Err(Missing::Tmux);
                }
                if f[0].is_empty() {
                    return Err(Missing::Pane);
                }
                read = Some(Read {
                    pane_pid: f[1].parse().map_err(|_| Missing::Tmux)?,
                    pane: Pane {
                        state: f[2].into(),
                        since: f[3].into(),
                        prev: f[4].into(),
                        needs_id: f[5].into(),
                        tool: f[6].into(),
                        tests_sound_at: f[7].into(),
                        sid: f[8].into(),
                        session: f[9].into(),
                        space: f[12].into(),
                        muted: !f[12].is_empty() && f[13] == "on",
                        agent: f[17].into(),
                        turn: f[18].into(),
                        transcript: f[19].into(),
                        agent_pid: f[20].into(),
                        agent_pid_start: f[21].into(),
                        bg: f[22].into(),
                        bg_watch: f[23].into(),
                        needs: f[26].into(),
                        title: f[27].into(),
                    },
                    window_active: f[10] == "1",
                    pane_active: f[11] == "1",
                    remind_after: f[14].into(),
                    test_regex: f[15].into(),
                    bin: f[16].into(),
                    sound: f[24].into(),
                    sound_volume: f[25].into(),
                    ..Read::default()
                });
            }
            "C" => {
                let f: Vec<&str> = rest.split(US).collect();
                if let [name, pid, flags, session, control] = f[..] {
                    clients.push(Client {
                        name: name.into(),
                        pid: pid.parse().unwrap_or(0),
                        flags: flags.into(),
                        session: session.into(),
                        control: control == "1",
                    });
                }
            }
            "P" => {
                let f: Vec<&str> = rest.splitn(5, US).collect();
                if let [pane, state, subs, sid, space] = f[..] {
                    others.push(OtherPane {
                        pane: pane.into(),
                        state: state.into(),
                        subs: subs.into(),
                        sid: sid.into(),
                        space: space.into(),
                    });
                }
            }
            _ => {}
        }
    }
    let mut read = read.ok_or(Missing::Tmux)?;
    read.clients = clients;
    read.others = others;
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tag: char, fields: &[&str]) -> String {
        format!("{RS}{tag}{US}{}\n", fields.join(&US.to_string()))
    }

    #[test]
    fn parses_pane_clients_and_panes() {
        let mut d = vec![""; 28];
        d[0] = "%3";
        d[1] = "4242";
        d[2] = "needs";
        d[3] = "1790000000";
        d[6] = "Bash: echo ##1";
        d[9] = "api";
        d[10] = "1";
        d[11] = "0";
        d[12] = "work";
        d[13] = "on";
        d[16] = "/x/bin";
        d[17] = "codex";
        d[20] = "4242";
        d[24] = "off";
        d[26] = "permission";
        d[27] = "✳ multi\nline";
        let out = record('D', &d)
            + &record(
                'C',
                &["/dev/pts/1", "77", "attached,focused,UTF-8", "api", "0"],
            )
            + &record('C', &["client-9", "9", "control-mode", "api", "1"])
            + &record('P', &["%3", "needs", "", "", "work"])
            + &record('P', &["%4", "done", "2", "s4", "home"]);
        let r = parse(&out).unwrap();
        assert_eq!(r.pane_pid, 4242);
        assert_eq!(r.pane.state, "needs");
        assert_eq!(r.pane.tool, "Bash: echo ##1");
        assert_eq!(r.pane.needs, "permission");
        assert_eq!(r.pane.title, "✳ multi\nline");
        assert!(r.pane.muted);
        assert!(r.window_active && !r.pane_active);
        assert_eq!(r.bin, "/x/bin");
        assert_eq!(r.sound, "off");
        assert_eq!(
            (r.pane.agent.as_str(), r.pane.agent_pid.as_str()),
            ("codex", "4242")
        );
        assert_eq!(r.clients.len(), 2);
        assert!(r.clients[1].control);
        assert_eq!(r.others[1].subs, "2");
        assert_eq!(r.others[1].sid, "s4");
        assert_eq!(r.others[1].space, "home");
    }

    #[test]
    fn mute_needs_a_space() {
        let mut d = vec![""; 28];
        d[0] = "%1";
        d[1] = "1";
        d[13] = "on";
        assert!(!parse(&record('D', &d)).unwrap().pane.muted);
    }

    #[test]
    fn no_pane_no_read() {
        assert_eq!(parse(""), Err(Missing::Tmux));
        assert_eq!(
            parse(&record('C', &["c", "1", "", "s", "0"])),
            Err(Missing::Tmux)
        );
        // tmux 3.6 "displays" a pane that is gone, with every field empty.
        assert_eq!(parse(&record('D', &[""; 28])), Err(Missing::Pane));
    }

    #[test]
    fn semicolons_survive() {
        assert_eq!(protect_semicolon("done;"), "done\\;");
        assert_eq!(protect_semicolon("a\\;"), "a\\\\;");
        assert_eq!(protect_semicolon(";"), "\\;");
        assert_eq!(protect_semicolon("a;b"), "a;b");
    }
}
