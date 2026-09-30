//! The server's end of a remote pane: `agentd hold [<name>]` and the holder
//! (design.md, "Remote panes").
//!
//! `agentd hold <name>` is what ssh runs: it joins its stdio to the holder of
//! `<name>`, starting it if there is none. The holder owns a pty and the
//! program in it, keeps its last output, serves one client at a time and
//! takes the hooks of the agents in it. Plain threads: a pty and a few
//! sockets, one client.

use std::collections::VecDeque;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{FlockOperation, Mode, OFlags, flock};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};
use serde::Deserialize;
use serde_json::json;

use super::frame::{
    self, ATTACHED, DETACHED, EVENT, EXIT, HELLO, HOOK, INPUT, OUTPUT, QUERY, RESIZE,
};
use super::{HOLD_VAR, Ring};
use crate::ownership;
use crate::procfs;
use crate::proto::{HookRequest, Remote};

/// What a new pane gets back of the program's screen.
const KEEP_OUTPUT: usize = 1 << 20;
/// Events kept while nobody is attached, and events of the agent's session
/// for a new pane; the oldest go first.
const KEEP_EVENTS: usize = 1000;
const KEEP_HISTORY: usize = 2000;
/// A program that exited waits this long for a client to hear it.
const KEEP_EXITED: Duration = Duration::from_secs(24 * 3600);
/// Connections at once (a thread each): a client, hooks, listings.
const MAX_CONNECTIONS: usize = 64;
/// Parent chain read for I4 (which looks at 13).
const MAX_CHAIN: usize = 16;
/// A client that takes longer to read a frame is dropped: it comes back and
/// gets what it missed from the buffer.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long `agentd hold <name>` waits for a holder it started.
const START_BUDGET: Duration = Duration::from_secs(3);
/// The first frame of a connection.
const FIRST_FRAME: Duration = Duration::from_secs(5);

/// `agentd hold` with no name: the held sessions, one per line.
pub fn list() -> ExitCode {
    let dir = super::dir();
    if super::ensure_dir(&dir).is_err() {
        return ExitCode::SUCCESS;
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        return ExitCode::SUCCESS;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let name = file.strip_prefix("hold-")?.strip_suffix(".sock")?;
            // Printed to a terminal: never a name we wouldn't make.
            super::valid_name(name).then(|| name.to_string())
        })
        .collect();
    names.sort();
    for name in names {
        let Some(status) = query(&super::socket(&dir, &name)) else {
            continue;
        };
        let what = if !status.running {
            "exited"
        } else if status.attached {
            "attached"
        } else {
            "detached"
        };
        println!("{name}\t{what}");
    }
    ExitCode::SUCCESS
}

#[derive(Deserialize)]
struct Status {
    attached: bool,
    running: bool,
}

fn query(socket: &Path) -> Option<Status> {
    let mut stream = super::connect(socket).ok()?;
    stream.set_read_timeout(Some(FIRST_FRAME)).ok()?;
    frame::write(&mut stream, QUERY, b"").ok()?;
    match frame::read_max(&mut stream, frame::SMALL).ok()? {
        Some((QUERY, p)) => serde_json::from_slice(&p).ok(),
        _ => None,
    }
}

/// `agentd hold <name>`: stdio joined to the holder of `<name>` until either
/// side ends.
pub fn attach(name: &str) -> ExitCode {
    let dir = super::dir();
    if let Err(e) = super::ensure_dir(&dir) {
        eprintln!("agentd hold: {e}");
        return ExitCode::FAILURE;
    }
    let socket = super::socket(&dir, name);
    if socket.as_os_str().len() > super::MAX_SOCKET_PATH {
        eprintln!(
            "agentd hold: {} is too long for a socket; a shorter TMUX_TMPDIR or name",
            socket.display()
        );
        return ExitCode::FAILURE;
    }
    let stream = match super::connect(&socket) {
        Ok(s) => s,
        Err(_) => match start(name, &socket) {
            Some(s) => s,
            None => {
                eprintln!("agentd hold: the holder of {name} did not start");
                return ExitCode::FAILURE;
            }
        },
    };
    let (Ok(mut up), Ok(stdin), Ok(stdout)) = (
        stream.try_clone(),
        io::stdin().as_fd().try_clone_to_owned(),
        io::stdout().as_fd().try_clone_to_owned(),
    ) else {
        return ExitCode::FAILURE;
    };
    thread::spawn(move || {
        pump(&mut File::from(stdin), &mut up);
        // ssh is gone: the holder hears it and lets this client go.
        let _ = up.shutdown(std::net::Shutdown::Write);
    });
    let mut down = stream;
    pump(&mut down, &mut File::from(stdout));
    ExitCode::SUCCESS
}

/// Bytes from `from` to `to` as they come, until either ends. Not
/// `io::copy`: on Linux it splices, and a splice from the socket waited
/// for more than the frame that was there.
fn pump(from: &mut impl Read, to: &mut impl Write) {
    let mut buf = vec![0u8; 64 << 10];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Starts the holder of `name` and waits for its socket.
fn start(name: &str, socket: &Path) -> Option<UnixStream> {
    Command::new(env::current_exe().ok()?)
        .args(["hold", "--serve", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + START_BUDGET;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
        if let Ok(s) = super::connect(socket) {
            return Some(s);
        }
    }
    None
}

/// `agentd hold --exec <pts> -- <program>...`: becomes the program, in a
/// session of its own with `<pts>` as its terminal. Our own binary does this
/// between fork and exec, which safe Rust can't.
pub fn exec(pts: &str, program: &[String]) -> ExitCode {
    let Some((first, args)) = program.split_first() else {
        return ExitCode::from(2);
    };
    let _ = rustix::process::setsid();
    // Opened by a session leader without a terminal, it becomes its terminal.
    let fd = match rustix::fs::open(pts, OFlags::RDWR, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("agentd hold: {pts}: {e}");
            return ExitCode::from(127);
        }
    };
    let _ = rustix::process::ioctl_tiocsctty(&fd);
    let dup = rustix::stdio::dup2_stdin(&fd)
        .and_then(|()| rustix::stdio::dup2_stdout(&fd))
        .and_then(|()| rustix::stdio::dup2_stderr(&fd));
    if dup.is_err() {
        return ExitCode::from(127);
    }
    drop(fd);
    let e = Command::new(first).args(args).exec();
    eprintln!("agentd hold: {first}: {e}");
    ExitCode::from(127)
}

/// `agentd hold --serve <name>`: the holder, until its program has exited
/// and a client has heard it (or nobody came for a day).
pub fn serve(name: &str) -> ExitCode {
    let _ = rustix::process::setsid();
    let dir = super::dir();
    if super::ensure_dir(&dir).is_err() {
        return ExitCode::FAILURE;
    }
    let lock_path = super::lock(&dir, name);
    let Ok(lock) = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
    else {
        return ExitCode::FAILURE;
    };
    // Another holder of this name runs, or is starting.
    if flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
        return ExitCode::SUCCESS;
    }
    let socket = super::socket(&dir, name);
    let _ = fs::remove_file(&socket);
    let Ok(listener) = UnixListener::bind(&socket) else {
        return ExitCode::FAILURE;
    };
    let _ = fs::set_permissions(&socket, fs::Permissions::from_mode(0o600));
    let holder = Arc::new(Holder {
        name: name.to_string(),
        socket,
        next: AtomicU64::new(0),
        connections: AtomicUsize::new(0),
        shared: Mutex::new(Shared {
            client: None,
            ring: Ring::new(KEEP_OUTPUT),
            events: VecDeque::new(),
            history: VecDeque::new(),
            program: None,
            alive: false,
            exit: None,
        }),
    });
    for stream in listener.incoming().flatten() {
        // A thread each, but not without end.
        if holder.connections.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
            holder.connections.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let holder = holder.clone();
        thread::spawn(move || {
            holder.clone().connection(stream);
            holder.connections.fetch_sub(1, Ordering::Relaxed);
        });
    }
    ExitCode::FAILURE
}

#[derive(Deserialize)]
struct Hello {
    #[serde(default)]
    term: String,
    rows: u16,
    cols: u16,
    have: Option<u64>,
    /// Where the program starts: `~` is this host's home.
    #[serde(default)]
    dir: Option<String>,
}

struct Holder {
    name: String,
    socket: PathBuf,
    next: AtomicU64,
    connections: AtomicUsize,
    shared: Mutex<Shared>,
}

struct Shared {
    /// The attached client, with its number.
    client: Option<(u64, UnixStream)>,
    ring: Ring,
    /// Events for the next client.
    events: VecDeque<HookRequest>,
    /// The events a client got since the agent's session started: a new
    /// pane gets them again, to show its state.
    history: VecDeque<HookRequest>,
    /// The pty's master and the program's pid, once started.
    program: Option<(File, u32)>,
    /// Not reaped yet: its pid is still its own.
    alive: bool,
    exit: Option<i32>,
}

impl Holder {
    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn connection(self: Arc<Self>, mut stream: UnixStream) {
        // The folder is private already; a peer of another user (a socket
        // passed on) is not served either.
        if super::same_user(&stream).is_err() {
            return;
        }
        let _ = stream.set_read_timeout(Some(FIRST_FRAME));
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        match frame::read_max(&mut stream, frame::SMALL) {
            Ok(Some((HELLO, p))) => {
                if let Ok(hello) = serde_json::from_slice::<Hello>(&p) {
                    let _ = stream.set_read_timeout(None);
                    self.attach(stream, hello);
                }
            }
            Ok(Some((HOOK, p))) => self.hook(stream, &p),
            Ok(Some((QUERY, _))) => {
                let status = {
                    let s = self.shared();
                    json!({"attached": s.client.is_some(), "running": s.exit.is_none()})
                };
                let _ = frame::write(&mut stream, QUERY, status.to_string().as_bytes());
            }
            _ => {}
        }
    }

    /// A client: the program's screen and events to it, its keys and sizes
    /// to the program, until it goes or another one comes.
    fn attach(self: &Arc<Self>, stream: UnixStream, hello: Hello) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let Ok(mut reader) = stream.try_clone() else {
            return;
        };
        let size = Winsize {
            ws_row: hello.rows.max(1),
            ws_col: hello.cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let input = {
            let mut s = self.shared();
            let new = s.program.is_none() && s.exit.is_none();
            if new {
                match self.start(&hello.term, hello.dir.as_deref(), size) {
                    Ok((master, child)) => {
                        let pid = child.id();
                        let output = master.try_clone();
                        s.program = Some((master, pid));
                        s.alive = true;
                        if let Ok(output) = output {
                            let holder = self.clone();
                            thread::spawn(move || holder.output(output));
                        }
                        let holder = self.clone();
                        thread::spawn(move || holder.wait(child));
                    }
                    Err(e) => {
                        let why = json!({"why": format!("could not start a shell: {e}")});
                        let mut stream = stream;
                        let _ = frame::write(&mut stream, DETACHED, why.to_string().as_bytes());
                        return;
                    }
                }
            }
            if let Some((_, mut old)) = s.client.take() {
                let why = json!({"why": "attached somewhere else"});
                let _ = frame::write(&mut old, DETACHED, why.to_string().as_bytes());
                let _ = old.shutdown(std::net::Shutdown::Both);
            }
            let (at, missed) = s.ring.since(if new { None } else { hello.have });
            // A new pane, or one we wrote "connection lost" on: a TUI has to
            // draw itself again.
            let redraw = !new;
            let mut stream = stream;
            let attached = json!({"new": new, "at": at});
            let replay: Vec<&HookRequest> = if hello.have.is_none() {
                s.history.iter().collect()
            } else {
                Vec::new()
            };
            let sent = frame::write(&mut stream, ATTACHED, attached.to_string().as_bytes())
                .and_then(|()| {
                    replay
                        .into_iter()
                        .try_for_each(|e| send_event(&mut stream, e, true))
                })
                .and_then(|()| {
                    s.events
                        .iter()
                        .try_for_each(|e| send_event(&mut stream, e, false))
                })
                .and_then(|()| {
                    missed
                        .chunks(64 << 10)
                        .try_for_each(|c| frame::write(&mut stream, OUTPUT, c))
                });
            if sent.is_err() {
                return;
            }
            let delivered: Vec<HookRequest> = s.events.drain(..).collect();
            for e in delivered {
                remember(&mut s, e);
            }
            if let Some(code) = s.exit {
                let _ = frame::write(
                    &mut stream,
                    EXIT,
                    json!({"code": code}).to_string().as_bytes(),
                );
                self.finish();
            }
            let master = s.program.as_ref().and_then(|(m, _)| m.try_clone().ok());
            if let Some(m) = &master {
                if redraw {
                    nudge(m, size);
                } else {
                    let _ = tcsetwinsize(m, size);
                }
            }
            s.client = Some((id, stream));
            master
        };
        let Some(mut input) = input else {
            return;
        };
        while let Ok(Some((kind, payload))) = frame::read_max(&mut reader, frame::SMALL) {
            match kind {
                INPUT => {
                    if input.write_all(&payload).is_err() {
                        break;
                    }
                }
                RESIZE => {
                    if let Ok(h) = serde_json::from_slice::<Hello>(&payload) {
                        let _ = tcsetwinsize(
                            &input,
                            Winsize {
                                ws_row: h.rows.max(1),
                                ws_col: h.cols.max(1),
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        let mut s = self.shared();
        if s.client.as_ref().is_some_and(|(c, _)| *c == id) {
            s.client = None;
        }
    }

    /// The program, in a new pty of the client's size, with the client's
    /// terminal type and `AGENTD_HOLD` for its agents' hooks.
    fn start(&self, term: &str, dir: Option<&str>, size: Winsize) -> io::Result<(File, Child)> {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
        grantpt(&master)?;
        unlockpt(&master)?;
        let pts = ptsname(&master, Vec::new())?;
        tcsetwinsize(&master, size)?;
        let shell = env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let home = PathBuf::from(env::var_os("HOME").unwrap_or_else(|| "/".into()));
        // A folder that isn't there (a typo, another host's layout): home.
        let cwd = dir
            .map(|d| match d.strip_prefix('~') {
                Some(rest) => home.join(rest.trim_start_matches('/')),
                None => PathBuf::from(d),
            })
            .filter(|d| d.is_dir())
            .unwrap_or_else(|| home.clone());
        let child = Command::new(env::current_exe()?)
            .arg("hold")
            .arg("--exec")
            .arg(OsStr::from_bytes(pts.as_bytes()))
            .arg("--")
            .arg(shell)
            .arg("-l")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env(HOLD_VAR, &self.socket)
            .env("AGENTD_HOLD_NAME", &self.name)
            .env("TERM", terminal(term))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok((File::from(master), child))
    }

    /// The program's output: kept, and sent to the client if there is one.
    fn output(&self, mut master: File) {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let n = match master.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // EIO: nothing has the terminal open any more.
                Err(_) => return,
            };
            let mut s = self.shared();
            s.ring.push(&buf[..n]);
            if let Some((_, client)) = s.client.as_mut()
                && frame::write(client, OUTPUT, &buf[..n]).is_err()
            {
                let _ = client.shutdown(std::net::Shutdown::Both);
                s.client = None;
            }
        }
    }

    /// The program's end: its status to the client, now or when one comes.
    fn wait(self: Arc<Self>, mut child: Child) {
        let code = match child.wait() {
            Ok(status) => status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            Err(_) => 1,
        };
        // Reaped: its pid may be another process's from now on, which a
        // hook's chain must not match.
        self.shared().alive = false;
        // Its last output, still on its way through the pty.
        thread::sleep(Duration::from_millis(100));
        let mut s = self.shared();
        s.exit = Some(code);
        s.program = None;
        if let Some((_, mut client)) = s.client.take() {
            let _ = frame::write(
                &mut client,
                EXIT,
                json!({"code": code}).to_string().as_bytes(),
            );
            self.finish();
        }
        drop(s);
        let holder = self.clone();
        thread::spawn(move || {
            thread::sleep(KEEP_EXITED);
            holder.finish();
        });
    }

    /// A hook of the held program (hook.rs): I4 against the program, on the
    /// chain of the process that connected as /proc has it (never the one it
    /// sends), then down to the client, or kept for the next one.
    fn hook(&self, mut stream: UnixStream, payload: &[u8]) {
        let reply = |stream: &mut UnixStream, v: serde_json::Value| {
            let _ = frame::write(stream, HOOK, v.to_string().as_bytes());
        };
        let Ok(mut request) = serde_json::from_slice::<HookRequest>(payload) else {
            return reply(&mut stream, json!({"ok": false, "error": "bad request"}));
        };
        // The hook waits for our answer: its pid is its own while we look.
        let Some(hook) = super::peer_pid(&stream) else {
            return reply(&mut stream, json!({"ok": false, "error": "no peer"}));
        };
        request.chain = procfs::chain(hook, MAX_CHAIN + 1)
            .into_iter()
            .skip(1)
            .map(|p| (p.pid, p.comm, p.starttime))
            .collect();
        request.event = std::mem::take(&mut request.event).from_remote();
        let mut s = self.shared();
        let Some(pid) = s.program.as_ref().filter(|_| s.alive).map(|(_, pid)| *pid) else {
            return reply(&mut stream, json!({"ok": false, "error": "no program"}));
        };
        let chain = request.chain.iter().map(|(p, c, _)| (*p, c.as_str()));
        let Some(agent) = ownership::owner(chain, pid) else {
            return reply(&mut stream, json!({"ok": false, "error": "not-its-agent"}));
        };
        request.remote = Some(Remote {
            host: String::new(),
            name: self.name.clone(),
            agent,
            replay: false,
        });
        let sent = match s.client.as_mut() {
            Some((_, client)) => send_event(client, &request, false).is_ok(),
            None => false,
        };
        if sent {
            remember(&mut s, request);
        } else {
            if let Some((_, client)) = s.client.take() {
                let _ = client.shutdown(std::net::Shutdown::Both);
            }
            if s.events.len() >= KEEP_EVENTS {
                s.events.pop_front();
            }
            s.events.push_back(request);
        }
        drop(s);
        reply(&mut stream, json!({"ok": true}));
    }

    fn finish(&self) -> ! {
        let _ = fs::remove_file(&self.socket);
        // The lock file stays: removed, a holder starting now could lock the
        // old one while the next locks a new one, two holders for a name.
        std::process::exit(0)
    }
}

fn send_event(stream: &mut UnixStream, event: &HookRequest, replay: bool) -> io::Result<()> {
    let mut event = event.clone();
    if let Some(remote) = event.remote.as_mut() {
        remote.replay = replay;
    }
    frame::write(stream, EVENT, &serde_json::to_vec(&event)?)
}

/// A delivered event, for the next new pane. The agent's (not a
/// subagent's) SessionStart begins them again: what came before it no
/// longer shows.
fn remember(s: &mut Shared, event: HookRequest) {
    if event.event.ev == "SessionStart" && event.event.agent_id.is_empty() {
        s.history.clear();
    }
    if s.history.len() >= KEEP_HISTORY {
        s.history.pop_front();
    }
    s.history.push_back(event);
}

/// Resizes the pty one column narrower and back: two SIGWINCH, and a TUI
/// draws its whole screen again.
fn nudge(master: &File, size: Winsize) {
    let narrower = Winsize {
        ws_col: size.ws_col.saturating_sub(1).max(1),
        ..size
    };
    let _ = tcsetwinsize(master, narrower);
    let Ok(master) = master.try_clone() else {
        return;
    };
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        let _ = tcsetwinsize(&master, size);
    });
}

/// The client's terminal type if this host knows it, else the nearest one it
/// does: a program without its terminfo draws nothing right.
fn terminal(term: &str) -> String {
    // A name, never a path: it is looked up in terminfo folders.
    let name = |t: &str| {
        !t.is_empty()
            && t.len() <= 64
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    };
    let wanted = if name(term) { term } else { "xterm-256color" };
    [wanted, "tmux-256color", "screen-256color", "xterm-256color"]
        .into_iter()
        .find(|t| has_terminfo(t))
        .unwrap_or(wanted)
        .to_string()
}

fn has_terminfo(term: &str) -> bool {
    let Some(first) = term.chars().next() else {
        return false;
    };
    let var = |name| env::var_os(name).filter(|v| !v.is_empty());
    let mut dirs: Vec<PathBuf> = Vec::new();
    dirs.extend(var("TERMINFO").map(PathBuf::from));
    dirs.extend(var("HOME").map(|h| Path::new(&h).join(".terminfo")));
    if let Some(list) = var("TERMINFO_DIRS") {
        dirs.extend(env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    dirs.extend(
        [
            "/etc/terminfo",
            "/lib/terminfo",
            "/usr/share/terminfo",
            "/usr/lib/terminfo",
        ]
        .map(PathBuf::from),
    );
    dirs.iter().any(|d| {
        d.join(first.to_string()).join(term).exists()
            || d.join(format!("{:x}", first as u32)).join(term).exists()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_terminal_is_kept() {
        // Every Linux box has xterm's.
        assert_eq!(terminal("xterm-256color"), "xterm-256color");
        assert_eq!(terminal(""), terminal("xterm-256color"));
        assert_ne!(terminal("no-such-terminal-x"), "no-such-terminal-x");
    }
}
