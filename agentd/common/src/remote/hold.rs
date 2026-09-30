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
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
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
/// Events kept, numbered: what a reconnecting client missed, and the agent's
/// session for a new pane. The oldest go first.
const KEEP_EVENTS: usize = 2000;
/// Frames waiting for a client; one that falls further behind is dropped
/// (it comes back and gets what it missed).
const MAX_QUEUED: usize = 4 << 20;
/// How long the program's last output may take to drain after its end.
const DRAIN: Duration = Duration::from_secs(1);
/// A program that exited waits this long for a client to hear it.
const KEEP_EXITED: Duration = Duration::from_secs(24 * 3600);
/// Connections at once (a thread each): a client, hooks, listings.
const MAX_CONNECTIONS: usize = 64;
/// Parent chain read for I4 (which looks at 13).
const MAX_CHAIN: usize = 16;
/// A client whose socket takes this long to take one frame is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
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
        drained: (Mutex::new(false), Condvar::new()),
        shared: Mutex::new(Shared {
            client: None,
            ring: Ring::new(KEEP_OUTPUT),
            log: VecDeque::new(),
            seq: 0,
            session: 1,
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
    /// The number of the last event it got; absent: a new pane.
    #[serde(default)]
    events: Option<u64>,
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
    /// Set when the pty has nothing more to read.
    drained: (Mutex<bool>, Condvar),
}

struct Shared {
    client: Option<Client>,
    ring: Ring,
    /// The last events, numbered from 1.
    log: VecDeque<HookRequest>,
    /// The number of the last event.
    seq: u64,
    /// Where the agent's session began in `log` (its SessionStart).
    session: u64,
    /// The pty's master and the program's pid, once started.
    program: Option<(File, u32)>,
    /// Not reaped yet: its pid is still its own.
    alive: bool,
    exit: Option<i32>,
}

/// The attached client. Frames go through its writer thread, so nothing
/// waits on its socket while holding `Shared`.
struct Client {
    id: u64,
    out: mpsc::Sender<Out>,
    queued: Arc<AtomicUsize>,
    stream: UnixStream,
}

enum Out {
    Frame(Vec<u8>),
    /// The program's end: once written, the holder's too.
    Exit(Vec<u8>),
}

impl Client {
    /// Queues a frame; `false` when the client is too far behind.
    fn send(&self, kind: u8, payload: &[u8]) -> bool {
        let Some(frame) = frame::encode(kind, payload) else {
            return false;
        };
        let n = frame.len();
        if self.queued.fetch_add(n, Ordering::Relaxed) + n > MAX_QUEUED {
            return false;
        }
        self.out.send(Out::Frame(frame)).is_ok()
    }

    fn event(&self, event: &HookRequest, replay: bool) -> bool {
        let mut event = event.clone();
        if let Some(remote) = event.remote.as_mut() {
            remote.replay = replay;
        }
        serde_json::to_vec(&event).is_ok_and(|e| self.send(EVENT, &e))
    }
}

impl Shared {
    /// The client goes now: its socket closes under its writer.
    fn drop_client(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = client.stream.shutdown(std::net::Shutdown::Both);
        }
    }
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
            Ok(Some((HELLO, p))) => match serde_json::from_slice::<Hello>(&p) {
                Ok(hello) => {
                    let _ = stream.set_read_timeout(None);
                    self.attach(stream, hello);
                }
                Err(_) => {
                    // Say why, or the client takes it for a dropped line.
                    let why = json!({"why": "the holder can't read this client's hello (agentd versions differ?)"});
                    let _ = frame::write(&mut stream, DETACHED, why.to_string().as_bytes());
                }
            },
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
        let (Ok(mut reader), Ok(writer)) = (stream.try_clone(), stream.try_clone()) else {
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
            // The one attached before: told, and let go once its writer has
            // sent that.
            if let Some(old) = s.client.take() {
                let why = json!({"why": "attached somewhere else"});
                old.send(DETACHED, why.to_string().as_bytes());
            }
            let (out, frames) = mpsc::channel();
            let queued = Arc::new(AtomicUsize::new(0));
            let holder = self.clone();
            let counted = queued.clone();
            thread::spawn(move || holder.writer(writer, frames, counted));
            let client = Client {
                id,
                out,
                queued,
                stream,
            };
            let (at, missed) = s.ring.since(if new { None } else { hello.have });
            // A new pane, or one we wrote "connection lost" on: a TUI has to
            // draw itself again.
            let redraw = !new;
            // And the number of the last event: what it has from now on.
            let attached = json!({"new": new, "at": at, "events": s.seq});
            let mut sent = client.send(ATTACHED, attached.to_string().as_bytes());
            match hello.events {
                // A new pane: the agent's session again, to show its state.
                None => {
                    for e in s.log.iter().filter(|e| seq_of(e) >= s.session) {
                        sent = sent && client.event(e, true);
                    }
                }
                // The same pane again: what it missed, as it happened.
                Some(have) => {
                    for e in s.log.iter().filter(|e| seq_of(e) > have) {
                        sent = sent && client.event(e, false);
                    }
                }
            }
            for chunk in missed.chunks(64 << 10) {
                sent = sent && client.send(OUTPUT, chunk);
            }
            if let Some(code) = s.exit {
                let exit = frame::encode(EXIT, json!({"code": code}).to_string().as_bytes());
                if let Some(exit) = exit {
                    let _ = client.out.send(Out::Exit(exit));
                }
            }
            let master = s.program.as_ref().and_then(|(m, _)| m.try_clone().ok());
            if let Some(m) = &master {
                if redraw {
                    nudge(m, size);
                } else {
                    let _ = tcsetwinsize(m, size);
                }
            }
            s.client = Some(client);
            if !sent {
                s.drop_client();
                return;
            }
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
        if s.client.as_ref().is_some_and(|c| c.id == id) {
            s.drop_client();
        }
    }

    /// A client's frames, in order, until its queue closes or it can't take
    /// one. After the program's end frame, the holder ends.
    fn writer(
        &self,
        mut stream: UnixStream,
        frames: mpsc::Receiver<Out>,
        queued: Arc<AtomicUsize>,
    ) {
        for out in frames {
            match out {
                Out::Frame(frame) => {
                    if stream.write_all(&frame).is_err() {
                        break;
                    }
                    queued.fetch_sub(frame.len(), Ordering::Relaxed);
                }
                Out::Exit(frame) => {
                    if stream
                        .write_all(&frame)
                        .and_then(|()| stream.flush())
                        .is_ok()
                    {
                        // Heard: nothing is left to hold. Taken so no other
                        // thread writes half a frame meanwhile.
                        let _s = self.shared();
                        self.finish();
                    }
                    break;
                }
            }
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
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
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // EIO: nothing has the terminal open any more.
                Err(_) => break,
            };
            let mut s = self.shared();
            s.ring.push(&buf[..n]);
            if s.client
                .as_ref()
                .is_some_and(|c| !c.send(OUTPUT, &buf[..n]))
            {
                s.drop_client();
            }
        }
        let (done, drained) = &self.drained;
        *done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        drained.notify_all();
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
        // Its last output, still in the pty (a process it left behind may
        // keep the terminal open: not forever).
        {
            let (done, drained) = &self.drained;
            let done = done.lock().unwrap_or_else(|e| e.into_inner());
            let _ = drained.wait_timeout_while(done, DRAIN, |d| !*d);
        }
        let mut s = self.shared();
        s.exit = Some(code);
        s.program = None;
        let exit = frame::encode(EXIT, json!({"code": code}).to_string().as_bytes());
        if let (Some(client), Some(exit)) = (s.client.as_ref(), exit) {
            // The writer ends the holder once it is written; if it can't be,
            // the next client hears it.
            let _ = client.out.send(Out::Exit(exit));
        }
        drop(s);
        let holder = self.clone();
        thread::spawn(move || {
            thread::sleep(KEEP_EXITED);
            let _s = holder.shared();
            holder.finish();
        });
    }

    /// A hook of the held program (hook.rs): I4 against the program, on the
    /// chain of the process that connected as /proc has it (never the one it
    /// sends), then numbered, kept, and down to the client if there is one.
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
        s.seq += 1;
        let seq = s.seq;
        request.remote = Some(Remote {
            host: String::new(),
            name: self.name.clone(),
            agent,
            replay: false,
            seq,
        });
        // A new session of the agent (not a compaction's SessionStart, which
        // goes on with the same one) is what a new pane shows from.
        let e = &request.event;
        if e.ev == "SessionStart" && e.agent_id.is_empty() && e.source != "compact" {
            s.session = seq;
        }
        if s.client.as_ref().is_some_and(|c| !c.event(&request, false)) {
            s.drop_client();
        }
        if s.log.len() >= KEEP_EVENTS {
            s.log.pop_front();
        }
        s.log.push_back(request);
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

fn seq_of(event: &HookRequest) -> u64 {
    event.remote.as_ref().map_or(0, |r| r.seq)
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
