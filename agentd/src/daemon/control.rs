//! The control-mode client (phase 4, spike-control-mode.md case k): one
//! `tmux -C` of our own, attached to a session of its own, `_peek-agentd`,
//! that every consumer already skips (contract Z1); commands go in as lines,
//! answers come back as `%begin`/`%end` blocks.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::rc::Rc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, mpsc, oneshot};

/// Our session: `_peek-` keeps it out of the bar, the board, the session
/// search, agent-next and the blink; destroy-unattached removes it when we go.
pub const SESSION: &str = "_peek-agentd";

/// What a request got back: the output of all its commands, or the error of
/// the one that failed (tmux runs no command after a failing one).
pub type Answer = Result<String, String>;

struct Pending {
    commands: usize,
    done: usize,
    out: String,
    reply: oneshot::Sender<Answer>,
}

pub struct Control {
    lines: mpsc::UnboundedSender<String>,
    pending: Rc<RefCell<VecDeque<Pending>>>,
    alive: Rc<Cell<bool>>,
    _child: Child,
}

impl Control {
    /// Attaches: our session (created if needed, gone when we detach), no
    /// pane output, never sizing a window, never the narrowest client.
    /// `sessions_changed` is told when a session is created or closed,
    /// `bar_changed` also when a window is added or closed (what the top row
    /// shows, bar.rs), and
    /// `messages` gets the text of every `display-message -c` to our client.
    pub fn start(
        socket: &Path,
        sessions_changed: Rc<Notify>,
        bar_changed: Rc<Notify>,
        messages: mpsc::UnboundedSender<String>,
    ) -> std::io::Result<Control> {
        let mut child = Command::new("tmux")
            // UTF-8 whatever the locale: a client tmux takes for ASCII gets
            // `_` for every control character and non-ASCII letter.
            .arg("-u")
            .arg("-S")
            .arg(socket)
            .arg("-C")
            .args(["new-session", "-A", "-s", SESSION, "cat", ";"])
            .args([
                "set",
                "-t",
                &format!("={SESSION}:"),
                "destroy-unattached",
                "on",
                ";",
            ])
            .args(["refresh-client", "-f", "no-output,ignore-size", ";"])
            .args(["refresh-client", "-C", "1000x100"])
            // Not nested in the tmux it talks to.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        let (lines, mut to_write) = mpsc::unbounded_channel::<String>();
        let pending: Rc<RefCell<VecDeque<Pending>>> = Rc::default();
        let alive = Rc::new(Cell::new(true));

        tokio::task::spawn_local(async move {
            while let Some(line) = to_write.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let (reader_pending, reader_alive) = (pending.clone(), alive.clone());
        tokio::task::spawn_local(async move {
            let mut reader = BufReader::new(stdout);
            let mut parser = Parser::default();
            let mut raw = Vec::new();
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&raw);
                let line = line.strip_suffix('\n').unwrap_or(&line);
                match parser.line(line) {
                    Some(Event::Block {
                        ours: true,
                        ok,
                        lines,
                    }) => deliver(&reader_pending, ok, lines),
                    Some(Event::SessionsChanged) => {
                        sessions_changed.notify_one();
                        bar_changed.notify_one();
                    }
                    Some(Event::Message(text)) => {
                        let _ = messages.send(text);
                    }
                    Some(Event::WindowsChanged) => bar_changed.notify_one(),
                    Some(Event::Exit) => break,
                    _ => {}
                }
            }
            reader_alive.set(false);
            for p in reader_pending.borrow_mut().drain(..) {
                let _ = p.reply.send(Err("control client gone".into()));
            }
        });
        Ok(Control {
            lines,
            pending,
            alive,
            _child: child,
        })
    }

    pub fn alive(&self) -> bool {
        self.alive.get()
    }

    /// Sends one line of `commands` commands; the answer comes back on the
    /// receiver, in the order sent.
    pub fn send(&self, line: String, commands: usize) -> Option<oneshot::Receiver<Answer>> {
        if !self.alive.get() {
            return None;
        }
        let (reply, answer) = oneshot::channel();
        self.pending.borrow_mut().push_back(Pending {
            commands,
            done: 0,
            out: String::new(),
            reply,
        });
        self.lines.send(line).ok()?;
        Some(answer)
    }

    /// A request went unanswered: the stream can't be trusted any more.
    pub fn kill(&self) {
        self.alive.set(false);
        for p in self.pending.borrow_mut().drain(..) {
            let _ = p.reply.send(Err("control client dropped".into()));
        }
    }
}

/// One of our blocks: to the oldest request still waiting.
fn deliver(pending: &RefCell<VecDeque<Pending>>, ok: bool, lines: Vec<String>) {
    let mut pending = pending.borrow_mut();
    let Some(p) = pending.front_mut() else { return };
    for l in &lines {
        p.out.push_str(l);
        p.out.push('\n');
    }
    if !ok {
        if let Some(p) = pending.pop_front() {
            let _ = p.reply.send(Err(p.out));
        }
        return;
    }
    p.done += 1;
    if p.done == p.commands
        && let Some(p) = pending.pop_front()
    {
        let _ = p.reply.send(Ok(p.out));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    /// A complete block: from our commands (flags 1) or not (the attach's).
    Block {
        ours: bool,
        ok: bool,
        lines: Vec<String>,
    },
    SessionsChanged,
    /// `%message <text>`: a `display-message -c` to our client.
    Message(String),
    /// A window added or closed, in any session.
    WindowsChanged,
    Exit,
}

/// Control-mode output, line by line. Output is not escaped, so a block ends
/// only at `%end`/`%error` with the very time, number and flags of its
/// `%begin`.
#[derive(Default)]
struct Parser {
    open: Option<(String, bool, Vec<String>)>,
}

impl Parser {
    fn line(&mut self, line: &str) -> Option<Event> {
        match &mut self.open {
            None => {
                if let Some(tag) = line.strip_prefix("%begin ") {
                    let ours = tag.rsplit(' ').next() == Some("1");
                    self.open = Some((tag.to_string(), ours, Vec::new()));
                    None
                } else if line == "%exit" || line.starts_with("%exit ") {
                    Some(Event::Exit)
                } else if line == "%sessions-changed" {
                    Some(Event::SessionsChanged)
                } else if [
                    "%window-add ",
                    "%window-close ",
                    "%unlinked-window-add ",
                    "%unlinked-window-close ",
                ]
                .iter()
                .any(|n| line.starts_with(n))
                {
                    Some(Event::WindowsChanged)
                } else {
                    // `%message`, or another notification.
                    line.strip_prefix("%message ")
                        .map(|text| Event::Message(text.to_string()))
                }
            }
            Some((tag, _, lines)) => {
                let end = line.strip_prefix("%end ").map(|t| (t, true));
                let error = line.strip_prefix("%error ").map(|t| (t, false));
                match end.or(error) {
                    Some((t, ok)) if t == tag => {
                        let (_, ours, lines) = self.open.take()?;
                        Some(Event::Block { ours, ok, lines })
                    }
                    _ => {
                        lines.push(line.to_string());
                        None
                    }
                }
            }
        }
    }
}

/// One word for the command parser: single-quoted, so `#`, `$`, `~`, `;`
/// and braces stay as they are.
pub fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The line for `commands`, or `None` when a word can't go on one line.
pub fn line(commands: &[Vec<String>]) -> Option<String> {
    if commands.iter().flatten().any(|w| w.contains(['\n', '\r'])) {
        return None;
    }
    let mut line = commands
        .iter()
        .map(|c| c.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(" ; ");
    line.push('\n');
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(p: &mut Parser, text: &str) -> Vec<Event> {
        text.lines().filter_map(|l| p.line(l)).collect()
    }

    #[test]
    fn t4_1_blocks_end_only_at_their_own_tag() {
        let mut p = Parser::default();
        let events = feed(
            &mut p,
            "%begin 1 277 0\n%end 1 277 0\n%session-changed $0 x\n%sessions-changed\n%message agentd seen %3\n%unlinked-window-close @4\n%begin 2 282 1\n%end 1 283 1\nvalue\n%end 2 282 1\n%begin 2 283 1\nno such pane: %9\n%error 2 283 1\n%exit\n",
        );
        assert_eq!(
            events,
            [
                Event::Block {
                    ours: false,
                    ok: true,
                    lines: vec![]
                },
                Event::SessionsChanged,
                Event::Message("agentd seen %3".into()),
                Event::WindowsChanged,
                // A forged end (another number) is content.
                Event::Block {
                    ours: true,
                    ok: true,
                    lines: vec!["%end 1 283 1".into(), "value".into()]
                },
                Event::Block {
                    ours: true,
                    ok: false,
                    lines: vec!["no such pane: %9".into()]
                },
                Event::Exit,
            ]
        );
    }

    #[test]
    fn t4_1_answers_by_order_and_count() {
        let pending: RefCell<VecDeque<Pending>> = RefCell::default();
        let (a, mut ra) = oneshot::channel();
        let (b, mut rb) = oneshot::channel();
        pending.borrow_mut().push_back(Pending {
            commands: 2,
            done: 0,
            out: String::new(),
            reply: a,
        });
        pending.borrow_mut().push_back(Pending {
            commands: 3,
            done: 0,
            out: String::new(),
            reply: b,
        });
        deliver(&pending, true, vec!["one".into()]);
        assert!(ra.try_recv().is_err(), "one of two");
        deliver(&pending, true, vec!["two".into()]);
        assert_eq!(ra.try_recv().unwrap(), Ok("one\ntwo\n".into()));
        // tmux skips what follows a failing command: its error ends the request.
        deliver(&pending, false, vec!["invalid option: @x".into()]);
        assert_eq!(rb.try_recv().unwrap(), Err("invalid option: @x\n".into()));
        assert!(pending.borrow().is_empty());
    }

    #[test]
    fn t4_1_quoting() {
        assert_eq!(quote("it's # $HOME ~ ; {x}"), r"'it'\''s # $HOME ~ ; {x}'");
        let cmds = vec![
            vec!["set".to_string(), "-p".into(), "@x".into(), "done;".into()],
            vec!["display".into(), "-p".into(), "#{pane_id}".into()],
        ];
        assert_eq!(
            line(&cmds).unwrap(),
            "'set' '-p' '@x' 'done;' ; 'display' '-p' '#{pane_id}'\n"
        );
        assert!(line(&[vec!["set".into(), "a\nb".into()]]).is_none());
    }
}
