//! The tmux transport of phases 1-3 (design.md, "tmux transport"): one
//! spawned `tmux` for everything an event reads, one for what it writes.

use std::borrow::Cow;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::core::{Op, OtherPane, Pane};

/// Record and field separators in tmux output: unlike tabs or newlines, no
/// name, title or option value we read has them.
const RS: char = '\x1e';
const US: char = '\x1f';

/// A wedged tmux server must not hold a pane's events forever.
const TIMEOUT: Duration = Duration::from_secs(3);

/// The session's space, as everywhere in agents/.
const SPACE: &str = "#{?@space,#{@space},#{@space_auto}}";

/// `@agent_mute_<space>`, the space sanitized as in `ag_space_muted`: the
/// option name is built by substitution and expanded a second time.
const MUTE: &str = "#{E:#{s/XSPACEX/#{s/[^A-Za-z0-9_-]/_/:#{?@space,#{@space},#{@space_auto}}}/:#{l:#{@agent_mute_XSPACEX}}}}";

/// The pane's fields, title last (it is the most likely to hold odd bytes).
const PANE_FIELDS: [&str; 19] = [
    "#{pane_id}",
    "#{pane_pid}",
    "#{@agent_state}",
    "#{@agent_since}",
    "#{@agent_prev}",
    "#{@agent_needs_id}",
    "#{@agent_tool}",
    "#{@agent_tests_sound_at}",
    "#{@agent_session}",
    "#{@agent_notify_id}",
    "#{session_name}",
    "#{window_active}",
    "#{pane_active}",
    SPACE,
    MUTE,
    "#{@agent_remind_after}",
    "#{@agent_test_regex}",
    "#{@agents_bin}",
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
    /// `@agent_notify_id`: the bash notifier has a notification open (phase 1).
    pub notify_id: String,
    pub remind_after: String,
    pub test_regex: String,
    /// `@agents_bin`, where the bash helpers are (phase 1).
    pub bin: String,
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
}

impl Tmux {
    pub fn new(socket: PathBuf, server_pid: u32) -> Self {
        let env = format!("{},{server_pid},0", socket.display());
        Tmux { socket, env }
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    /// Runs `tmux -S <socket> <args>`; its output if it succeeded.
    pub async fn run(&self, args: &[Cow<'_, str>]) -> Result<String, Missing> {
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
        } else if String::from_utf8_lossy(&out.stderr).contains("no such pane") {
            Err(Missing::Pane)
        } else {
            Err(Missing::Tmux)
        }
    }

    /// One `tmux` for the pane (P2 options, V2 facts, space, mute, §13
    /// globals), and when asked the clients (V1) and every pane (R).
    pub async fn read(&self, pane: &str, clients: bool, panes: bool) -> Result<Read, Missing> {
        let mut args: Vec<Cow<str>> = vec![
            "display".into(),
            "-p".into(),
            "-t".into(),
            pane.into(),
            format!("{RS}D{US}{}", PANE_FIELDS.join(&US.to_string())).into(),
        ];
        if clients {
            args.extend([
                ";".into(),
                "list-clients".into(),
                "-F".into(),
                format!("{RS}C{US}#{{client_name}}{US}#{{client_pid}}{US}#{{client_flags}}{US}#{{client_session}}{US}#{{client_control_mode}}").into(),
            ]);
        }
        if panes {
            args.extend([
                ";".into(),
                "list-panes".into(),
                "-a".into(),
                "-F".into(),
                format!(
                    "{RS}P{US}#{{pane_id}}{US}#{{@agent_state}}{US}#{{@agent_subs}}{US}{SPACE}"
                )
                .into(),
            ]);
        }
        parse(&self.run(&args).await?)
    }

    /// The pane's option writes, in order, in one `tmux`.
    pub async fn write(&self, pane: &str, ops: &[Op]) -> bool {
        if ops.is_empty() {
            return true;
        }
        let mut args: Vec<Cow<str>> = Vec::new();
        for op in ops {
            if !args.is_empty() {
                args.push(";".into());
            }
            match op {
                Op::Set(name, value) => {
                    args.extend(["set".into(), "-p".into(), "-t".into(), pane.into()]);
                    args.extend([(*name).into(), protect_semicolon(value)]);
                }
                Op::Unset(name) => {
                    args.extend(["set".into(), "-pu".into(), "-t".into(), pane.into()]);
                    args.push((*name).into());
                }
            }
        }
        self.run(&args).await.is_ok()
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
                        session: f[10].into(),
                        space: f[13].into(),
                        muted: !f[13].is_empty() && f[14] == "on",
                        title: f[18].into(),
                    },
                    notify_id: f[9].into(),
                    window_active: f[11] == "1",
                    pane_active: f[12] == "1",
                    remind_after: f[15].into(),
                    test_regex: f[16].into(),
                    bin: f[17].into(),
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
                let f: Vec<&str> = rest.splitn(4, US).collect();
                if let [pane, state, subs, space] = f[..] {
                    others.push(OtherPane {
                        pane: pane.into(),
                        state: state.into(),
                        subs: subs.into(),
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
        let mut d = vec![""; 19];
        d[0] = "%3";
        d[1] = "4242";
        d[2] = "needs";
        d[3] = "1790000000";
        d[6] = "Bash: echo ##1";
        d[10] = "api";
        d[11] = "1";
        d[12] = "0";
        d[13] = "work";
        d[14] = "on";
        d[17] = "/x/bin";
        d[18] = "✳ multi\nline";
        let out = record('D', &d)
            + &record(
                'C',
                &["/dev/pts/1", "77", "attached,focused,UTF-8", "api", "0"],
            )
            + &record('C', &["client-9", "9", "control-mode", "api", "1"])
            + &record('P', &["%3", "needs", "", "work"])
            + &record('P', &["%4", "done", "2", "home"]);
        let r = parse(&out).unwrap();
        assert_eq!(r.pane_pid, 4242);
        assert_eq!(r.pane.state, "needs");
        assert_eq!(r.pane.tool, "Bash: echo ##1");
        assert_eq!(r.pane.title, "✳ multi\nline");
        assert!(r.pane.muted);
        assert!(r.window_active && !r.pane_active);
        assert_eq!(r.bin, "/x/bin");
        assert_eq!(r.clients.len(), 2);
        assert!(r.clients[1].control);
        assert_eq!(r.others[1].subs, "2");
    }

    #[test]
    fn mute_needs_a_space() {
        let mut d = vec![""; 19];
        d[0] = "%1";
        d[1] = "1";
        d[14] = "on";
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
        assert_eq!(parse(&record('D', &[""; 19])), Err(Missing::Pane));
    }

    #[test]
    fn semicolons_survive() {
        assert_eq!(protect_semicolon("done;"), "done\\;");
        assert_eq!(protect_semicolon("a\\;"), "a\\\\;");
        assert_eq!(protect_semicolon(";"), "\\;");
        assert_eq!(protect_semicolon("a;b"), "a;b");
    }
}
