//! `agentd remote <host> <name>`: the local end of a remote pane
//! (design.md, "Remote panes"). The terminal in raw mode, joined over ssh to
//! the holder of `<name>` on `<host>`; the held agents' events to the local
//! daemon as this pane's own (I6). It reconnects when ssh drops.

use std::cell::Cell;
use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::termios::{OptionalActions, Termios, isatty, tcgetattr, tcgetwinsize, tcsetattr};
use serde::Deserialize;
use serde_json::json;

use super::frame::{self, ATTACHED, DETACHED, EVENT, EXIT, HELLO, INPUT, OUTPUT, RESIZE};
use crate::client;
use crate::procfs;
use crate::proto::{CtlRequest, HookRequest, Link, Request, VERSION};

/// The first wait before trying again, doubled each time up to the last.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_LAST: Duration = Duration::from_secs(30);
/// As hook.rs: how long an event waits for a daemon that has to start, and
/// for its ack.
const START_BUDGET: Duration = Duration::from_millis(300);
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(5);
/// I4 looks at 13; the chain starts at this process.
const MAX_CHAIN: usize = 16;
/// How long the events still queued at the end may take to reach the daemon.
const FLUSH_EVENTS: Duration = Duration::from_secs(2);
/// Events waiting for the local daemon; more are dropped.
const EVENTS_QUEUED: usize = 256;
const KEYS_POLL: Duration = Duration::from_millis(100);
/// While the pane is offline: the pane's colors, and what the status line says.
const OFFLINE_STYLE: &str = "bg=#2a1618";

/// Where the session is, for the keys.
/// Before the first attach the terminal is cooked and its keys are left
/// alone: ssh may be asking for a password or a host key on it.
const CONNECTING: u8 = 0;
/// Keys go to the holder.
const CONNECTED: u8 = 1;
/// Lost after an attach: still raw, since echo and our messages would
/// scramble the screen a TUI keeps track of; ctrl-c or ctrl-d gives up.
const OFFLINE: u8 = 2;

pub fn run(host: &str, name: &str, dir: Option<&str>) -> ExitCode {
    if !valid_host(host) {
        eprintln!("agentd remote: {host:?} is not a host");
        return ExitCode::from(2);
    }
    if !super::valid_name(name) {
        eprintln!("agentd remote: a name is letters, digits, '.', '_' and '-'");
        return ExitCode::from(2);
    }
    let Ok(stdin) = io::stdin().as_fd().try_clone_to_owned() else {
        return ExitCode::FAILURE;
    };
    if !isatty(&stdin) {
        eprintln!("agentd remote: needs a terminal");
        return ExitCode::FAILURE;
    }
    let pane = Pane::from_env(host, name);
    pane.mark();
    let mut session = Session::new(host, name, stdin, pane.clone());
    session.dir = dir.map(str::to_string);
    let code = session.run();
    pane.unmark();
    code
}

/// The local tmux pane this runs in, if any.
#[derive(Clone)]
struct Pane {
    /// `$TMUX` and `$TMUX_PANE`.
    tmux: Option<(String, String)>,
    mark: String,
}

impl Pane {
    fn from_env(host: &str, name: &str) -> Pane {
        let var = |n| env::var(n).ok().filter(|v| !v.is_empty());
        Pane {
            tmux: var("TMUX").zip(var("TMUX_PANE")),
            mark: format!("{host}:{name}"),
        }
    }

    /// `@agent_remote`: reconcile leaves the pane to us (I6).
    fn mark(&self) {
        if let Some((_, pane)) = &self.tmux {
            tmux(&["set", "-p", "-t", pane, "@agent_remote", &self.mark]);
        }
    }

    /// Gone from the pane: reconcile clears what its agent left.
    fn unmark(&self) {
        if let Some((_, pane)) = &self.tmux {
            tmux(&["set", "-pu", "-t", pane, "@agent_remote"]);
            self.reconcile();
        }
    }

    /// The pane's look while the connection is gone: tinted, and said once
    /// in the status line. Outside tmux, a line in the terminal.
    fn offline(&self, on: bool, what: &str) {
        let Some((_, pane)) = &self.tmux else {
            if on {
                eprint!("\r\n[{what}]\r\n");
            }
            return;
        };
        for option in ["window-style", "window-active-style"] {
            if on {
                tmux(&["set", "-p", "-t", pane, option, OFFLINE_STYLE]);
            } else {
                tmux(&["set", "-pu", "-t", pane, option]);
            }
        }
        if on {
            tmux(&["display-message", "-d", "5000", "-t", pane, what]);
        }
    }

    fn reconcile(&self) {
        let Some((tmux, pane)) = &self.tmux else {
            return;
        };
        let Some(paths) = client::paths(tmux.as_ref()) else {
            return;
        };
        let Ok(stream) = std::os::unix::net::UnixStream::connect(&paths.socket) else {
            return;
        };
        let request = Request::Ctl(CtlRequest {
            v: VERSION,
            ctl: "reconcile".into(),
            args: vec![pane.clone()],
        });
        let _ = client::call(stream, &request, RECONCILE_TIMEOUT);
    }
}

fn tmux(args: &[&str]) {
    let _ = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[derive(Deserialize)]
struct Attached {
    new: bool,
    at: u64,
    /// The number of the holder's last event.
    #[serde(default)]
    events: u64,
}

/// How a connection ended.
enum End {
    Exit(i32),
    Detached(String),
    Lost,
}

struct Session {
    host: String,
    name: String,
    /// Where the shell starts, if the holder starts it.
    dir: Option<String>,
    stdin: OwnedFd,
    pane: Pane,
    /// Where keys and sizes go: ssh's stdin, while connected.
    up: Arc<Mutex<Option<ChildStdin>>>,
    mode: Arc<AtomicU8>,
    quit: Arc<AtomicBool>,
    /// The number of the last event the holder sent us (`None`: a new pane).
    events_have: Cell<Option<u64>>,
    /// The transport while it runs, for a quit to end it: killed through
    /// its handle, never a pid that may have been reaped and reused.
    transport: Arc<Mutex<Option<Child>>>,
}

impl Session {
    fn new(host: &str, name: &str, stdin: OwnedFd, pane: Pane) -> Session {
        Session {
            host: host.to_string(),
            name: name.to_string(),
            dir: None,
            stdin,
            pane,
            up: Arc::default(),
            mode: Arc::default(),
            quit: Arc::default(),
            transport: Arc::default(),
            events_have: Cell::new(None),
        }
    }

    fn run(self) -> ExitCode {
        let (events, sent) = self.events();
        let code = self.session(&events);
        // What the holder sent reaches the daemon before the pane's last
        // reconcile (unmark): a late event would set its state again.
        drop(events);
        let _ = sent.recv_timeout(FLUSH_EVENTS);
        code
    }

    fn session(&self, events: &SyncSender<Vec<u8>>) -> ExitCode {
        let Ok(cooked) = tcgetattr(&self.stdin) else {
            return ExitCode::FAILURE;
        };
        let Ok(stdout) = io::stdout().as_fd().try_clone_to_owned() else {
            return ExitCode::FAILURE;
        };
        let mut out = File::from(stdout);
        let (wake, woken) = mpsc::channel::<()>();
        self.keys(wake.clone());
        self.signals(wake);
        let mut have: Option<u64> = None;
        let mut attached_before = false;
        let mut offline = false;
        let mut retry = RETRY_FIRST;
        let restore = || {
            let _ = tcsetattr(&self.stdin, OptionalActions::Now, &cooked);
        };
        loop {
            // After the first attach nothing may ask on the terminal: its keys
            // are read for ctrl-c, and would be taken from ssh's prompt.
            let end = match self.transport(attached_before) {
                Ok(child) => self.connection(
                    child,
                    &cooked,
                    &mut out,
                    events,
                    &mut have,
                    &mut attached_before,
                    &mut retry,
                    &mut offline,
                ),
                Err(e) => {
                    eprint!("agentd remote: {e}\r\n");
                    End::Lost
                }
            };
            let quit = self.quit.load(Ordering::Relaxed);
            if !matches!(end, End::Lost) || quit {
                restore();
                if offline {
                    self.pane.offline(false, "");
                }
            }
            match end {
                End::Exit(code) => return ExitCode::from(code.clamp(0, 255) as u8),
                End::Detached(why) => {
                    eprint!("\r\n[{}@{}: {why}]\r\n", self.name, self.host);
                    return ExitCode::SUCCESS;
                }
                End::Lost if quit => return ExitCode::SUCCESS,
                End::Lost => {}
            }
            let what = format!(
                "{}@{}: connection lost, retrying · ctrl-c in the pane gives up, it stays held there",
                self.name, self.host
            );
            if !attached_before {
                restore();
                eprint!("\r\n[{what}]\r\n");
            } else if !offline {
                self.mode.store(OFFLINE, Ordering::Relaxed);
                self.pane.offline(true, &what);
                offline = true;
            }
            match woken.recv_timeout(retry) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => {
                    restore();
                    if offline {
                        self.pane.offline(false, "");
                    }
                    return ExitCode::SUCCESS;
                }
            }
            retry = (retry * 2).min(RETRY_LAST);
        }
    }

    fn transport(&self, batch: bool) -> io::Result<Child> {
        let mut child = hold_command(&self.host, Some(&self.name), batch)?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // ssh's complaints: in the pane before the first attach (why it
        // can't connect), logged after it (the screen is a program's then).
        if let Some(stderr) = child.stderr.take() {
            let (mode, host) = (self.mode.clone(), self.host.clone());
            thread::spawn(move || {
                for line in io::BufRead::lines(io::BufReader::new(stderr)).map_while(Result::ok) {
                    if mode.load(Ordering::Relaxed) == CONNECTING {
                        eprint!("{line}\r\n");
                    } else {
                        crate::debug::log(&format!("agentd remote {host}: {line}"));
                    }
                }
            });
        }
        Ok(child)
    }

    #[allow(clippy::too_many_arguments)]
    fn connection(
        &self,
        mut child: Child,
        cooked: &Termios,
        out: &mut File,
        events: &SyncSender<Vec<u8>>,
        have: &mut Option<u64>,
        attached_before: &mut bool,
        retry: &mut Duration,
        offline: &mut bool,
    ) -> End {
        let (Some(mut up), Some(mut down)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return End::Lost;
        };
        *lock(&self.transport) = Some(child);
        let (rows, cols) = self.size();
        // A quit that came while it was starting found nothing to stop.
        let quit = self.quit.load(Ordering::Relaxed);
        let hello = json!({
            "term": env::var("TERM").unwrap_or_default(),
            "rows": rows,
            "cols": cols,
            "have": *have,
            "events": self.events_have.get(),
            "dir": self.dir,
        });
        let end = if quit || frame::write(&mut up, HELLO, hello.to_string().as_bytes()).is_err() {
            End::Lost
        } else {
            *self.lock_up() = Some(up);
            loop {
                match frame::read(&mut down) {
                    Ok(Some((ATTACHED, p))) => {
                        let Ok(a) = serde_json::from_slice::<Attached>(&p) else {
                            // Not a dropped line: trying again won't help.
                            break End::Detached(
                                "can't read the holder's answer (agentd versions differ?)".into(),
                            );
                        };
                        let mut raw = cooked.clone();
                        raw.make_raw();
                        let _ = tcsetattr(&self.stdin, OptionalActions::Now, &raw);
                        if a.new && *attached_before {
                            // The old program is gone (the host rebooted?):
                            // what the pane showed of its agent goes too.
                            self.pane.unmark();
                            self.pane.mark();
                            let _ = write!(
                                out,
                                "\r\n[{}@{}: a new shell, the old one is gone]\r\n",
                                self.name, self.host
                            );
                        }
                        *have = Some(a.at);
                        // A new pane counts from here (it got the agent's
                        // session as replay); a pane back counts on from
                        // the events it gets, in case the line drops again.
                        if self.events_have.get().is_none() {
                            self.events_have.set(Some(a.events));
                        }
                        *attached_before = true;
                        *retry = RETRY_FIRST;
                        if *offline {
                            self.pane.offline(false, "");
                            *offline = false;
                        }
                        self.mode.store(CONNECTED, Ordering::Relaxed);
                        // It may have changed since the hello.
                        self.resize();
                    }
                    Ok(Some((OUTPUT, p))) => {
                        if out.write_all(&p).is_err() {
                            break End::Lost;
                        }
                        *have = have.map(|h| h.saturating_add(p.len() as u64));
                    }
                    Ok(Some((EVENT, p))) => {
                        let seq = serde_json::from_slice::<serde_json::Value>(&p)
                            .ok()
                            .and_then(|v| v["remote"]["seq"].as_u64());
                        if let Some(seq) = seq {
                            let seen = self.events_have.get().unwrap_or(0);
                            self.events_have.set(Some(seen.max(seq)));
                        }
                        // Full: the daemon is behind (or the holder floods).
                        let _ = events.try_send(p);
                    }
                    Ok(Some((EXIT, p))) => {
                        let code = serde_json::from_slice::<serde_json::Value>(&p)
                            .ok()
                            .and_then(|v| v["code"].as_i64())
                            .unwrap_or(0);
                        // A status is 0..=255; anything else is a failure.
                        break End::Exit(i32::try_from(code).unwrap_or(1));
                    }
                    Ok(Some((DETACHED, p))) => {
                        let why = serde_json::from_slice::<serde_json::Value>(&p)
                            .ok()
                            .and_then(|v| v["why"].as_str().map(str::to_string))
                            .unwrap_or_else(|| "detached".into());
                        break End::Detached(why);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break End::Lost,
                }
            }
        };
        if self.mode.load(Ordering::Relaxed) == CONNECTED {
            self.mode.store(OFFLINE, Ordering::Relaxed);
        }
        *self.lock_up() = None;
        if let Some(mut child) = lock(&self.transport).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        end
    }

    fn lock_up(&self) -> std::sync::MutexGuard<'_, Option<ChildStdin>> {
        lock(&self.up)
    }

    fn size(&self) -> (u16, u16) {
        tcgetwinsize(&self.stdin).map_or((24, 80), |w| (w.ws_row, w.ws_col))
    }

    fn resize(&self) {
        let (rows, cols) = self.size();
        send(
            &self.up,
            RESIZE,
            json!({"rows": rows, "cols": cols}).to_string().as_bytes(),
        );
    }

    /// Keys to the holder while it is there; offline, only ctrl-c or
    /// ctrl-d, which give up.
    fn keys(&self, wake: Sender<()>) {
        let (Ok(stdin), up, mode, quit, transport) = (
            self.stdin.try_clone(),
            self.up.clone(),
            self.mode.clone(),
            self.quit.clone(),
            self.transport.clone(),
        ) else {
            return;
        };
        thread::spawn(move || {
            let mut buf = vec![0u8; 16 << 10];
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: KEYS_POLL.as_nanos() as i64,
            };
            while !quit.load(Ordering::Relaxed) {
                let now = mode.load(Ordering::Relaxed);
                if now == CONNECTING {
                    thread::sleep(KEYS_POLL);
                    continue;
                }
                let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
                if poll(&mut fds, Some(&timeout)).unwrap_or(0) == 0 {
                    continue;
                }
                if fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR) {
                    return;
                }
                let n = match rustix::io::read(&stdin, &mut buf) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => continue,
                    Err(_) => return,
                };
                if now == CONNECTED {
                    send(&up, INPUT, &buf[..n]);
                } else if buf[..n].iter().any(|&b| b == 0x03 || b == 0x04) {
                    give_up(&quit, &transport, &wake);
                    return;
                }
            }
        });
    }

    /// Window sizes to the holder; ctrl-c (the terminal is cooked while
    /// connecting), a hangup or a TERM give up.
    fn signals(&self, wake: Sender<()>) {
        let (up, mode, quit, transport, stdin) = (
            self.up.clone(),
            self.mode.clone(),
            self.quit.clone(),
            self.transport.clone(),
            self.stdin.try_clone(),
        );
        let Ok(stdin) = stdin else {
            return;
        };
        thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                use tokio::signal::unix::{SignalKind, signal};
                let (Ok(mut winch), Ok(mut int), Ok(mut hup), Ok(mut term)) = (
                    signal(SignalKind::window_change()),
                    signal(SignalKind::interrupt()),
                    signal(SignalKind::hangup()),
                    signal(SignalKind::terminate()),
                ) else {
                    return;
                };
                loop {
                    tokio::select! {
                        _ = winch.recv() => {
                            if mode.load(Ordering::Relaxed) == CONNECTED {
                                let (rows, cols) = tcgetwinsize(&stdin)
                                    .map_or((24, 80), |w| (w.ws_row, w.ws_col));
                                let size = json!({"rows": rows, "cols": cols}).to_string();
                                send(&up, RESIZE, size.as_bytes());
                            }
                        }
                        _ = int.recv() => break,
                        _ = hup.recv() => break,
                        _ = term.recv() => break,
                    }
                }
                // ssh goes with the connection: closing its stdin ends it.
                *up.lock().unwrap_or_else(|e| e.into_inner()) = None;
                give_up(&quit, &transport, &wake);
            });
        });
    }

    /// The held agents' events to the local daemon, in order, as the pane's
    /// own (I6).
    /// The second: closed once they are all sent.
    fn events(&self) -> (SyncSender<Vec<u8>>, Receiver<()>) {
        // Bounded: a holder that floods events can't grow it (they are
        // dropped, and the pane catches up at the agent's next one).
        let (tx, rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::sync_channel(EVENTS_QUEUED);
        let (done, sent) = mpsc::channel::<()>();
        let Some((tmux, pane)) = self.pane.tmux.clone() else {
            return (tx, sent);
        };
        let host = self.host.clone();
        thread::spawn(move || {
            let _done = done;
            let chain: Vec<Link> = procfs::chain(std::process::id(), MAX_CHAIN)
                .into_iter()
                .map(|s| (s.pid, s.comm, s.starttime))
                .collect();
            let Some(paths) = client::paths(tmux.as_ref()) else {
                return;
            };
            for event in rx {
                let Ok(mut request) = serde_json::from_slice::<HookRequest>(&event) else {
                    continue;
                };
                let Some(remote) = request.remote.as_mut() else {
                    continue;
                };
                if !super::valid_name(&remote.name) {
                    continue;
                }
                remote.host = host.clone();
                request.pane = pane.clone();
                request.chain = chain.clone();
                request.parked = None;
                let request = Request::Hook(Box::new(request));
                let sent = client::connect_or_start(&paths, START_BUDGET)
                    .ok_or_else(|| io::Error::other("no daemon"))
                    .and_then(|stream| client::call(stream, &request, REPLY_TIMEOUT));
                // Lost: the pane shows the agent's state again at its next event.
                if let Err(e) = sent {
                    crate::debug::log(&format!("agentd remote {host}: an event was lost: {e}"));
                }
            }
        });
        (tx, sent)
    }
}

/// `agentd remote <host>`: the shells held there.
pub fn list(host: &str) -> ExitCode {
    if !valid_host(host) {
        eprintln!("agentd remote: {host:?} is not a host");
        return ExitCode::from(2);
    }
    match hold_command(host, None, true).and_then(|mut c| c.status()) {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("agentd remote: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `agentd hold [name]` on the host, over ssh; `-` runs it here.
fn hold_command(host: &str, name: Option<&str>, batch: bool) -> io::Result<Command> {
    if host == "-" {
        let mut c = Command::new(env::current_exe()?);
        c.arg("hold").args(name);
        return Ok(c);
    }
    let var = |n| env::var_os(n).filter(|v| !v.is_empty());
    let agentd = var("AGENTD_REMOTE_AGENTD").map(|v| v.to_string_lossy().into_owned());
    let mut c = Command::new(var("AGENTD_SSH").unwrap_or_else(|| "ssh".into()));
    if batch {
        // Unattended (the prefix N form's listing, a reconnect): a prompt
        // would stop it for good, or fight us for the terminal's keys.
        c.args(["-o", "BatchMode=yes"]);
    }
    // No forwards: the desktop bridge's RemoteForward belongs to the
    // interactive ssh, and a second one would only fail to bind.
    c.args([
        "-T",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=3",
        // The host is never an option, whatever it looks like.
        "--",
        host,
    ]);
    c.arg(remote_script(agentd.as_deref(), name));
    Ok(c)
}

/// What ssh runs on the host: `AGENTD_REMOTE_AGENTD` if set, else the small
/// agentd-server, else a full agentd. A non-login shell runs it, so
/// ~/.local/bin (setup.sh's place) may not be in its PATH. Neither there:
/// exit 127 with a line that says so.
fn remote_script(agentd: Option<&str>, name: Option<&str>) -> String {
    let name = name.unwrap_or("");
    let path = "PATH=\"$HOME/.local/bin:$PATH\"";
    match agentd {
        Some(agentd) => format!("{path} exec {agentd} hold {name}"),
        None => format!(
            "{path}; a=$(command -v agentd-server || command -v agentd) || \
             {{ echo \"no agentd-server (or agentd) on $(hostname)\" >&2; exit 127; }}; \
             exec \"$a\" hold {name}"
        ),
    }
}

/// Stops trying: the transport, if one is connecting, and the wait.
fn give_up(quit: &AtomicBool, transport: &Mutex<Option<Child>>, wake: &Sender<()>) {
    quit.store(true, Ordering::Relaxed);
    if let Some(child) = lock(transport).as_mut() {
        let _ = child.kill();
    }
    let _ = wake.send(());
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A host as ssh takes it, or `-` for this one: never something ssh would
/// read as an option (`-oProxyCommand=...` runs a local command).
pub fn valid_host(host: &str) -> bool {
    host == "-"
        || (!host.is_empty()
            && !host.starts_with('-')
            && !host.chars().any(|c| c.is_whitespace() || c.is_control()))
}

/// One frame up, if connected; a failure shows as the connection ending.
fn send(up: &Mutex<Option<ChildStdin>>, kind: u8, payload: &[u8]) {
    let mut up = up.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(w) = up.as_mut()
        && frame::write(w, kind, payload).is_err()
    {
        *up = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the script in `sh` with a home whose ~/.local/bin has `bins`,
    /// each printing its own name: what it would exec.
    fn run(bins: &[&str], agentd: Option<&str>) -> (i32, String, String) {
        let home = std::env::temp_dir().join(format!(
            "agentd-script-{}-{}",
            std::process::id(),
            bins.join("-")
        ));
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        for b in bins {
            let f = bin.join(b);
            std::fs::write(&f, format!("#!/bin/sh\necho {b} \"$@\"\n")).unwrap();
            Command::new("chmod").arg("755").arg(&f).status().unwrap();
        }
        let out = Command::new("sh")
            .arg("-c")
            .arg(remote_script(agentd, Some("api")))
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        std::fs::remove_dir_all(&home).unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    }

    #[test]
    fn the_host_runs_the_small_binary_else_the_full_one() {
        assert_eq!(
            run(&["agentd-server", "agentd"], None).1,
            "agentd-server hold api"
        );
        assert_eq!(run(&["agentd"], None).1, "agentd hold api");
        let (code, out, err) = run(&[], None);
        assert_eq!((code, out.as_str()), (127, ""));
        assert!(err.starts_with("no agentd-server (or agentd) on "), "{err}");
        // Named: that one, found in ~/.local/bin too.
        assert_eq!(run(&["mine"], Some("mine")).1, "mine hold api");
    }

    #[test]
    fn a_host_is_never_an_option() {
        for ok in ["-", "box", "user@box", "box:22", "10.0.0.1"] {
            assert!(valid_host(ok), "{ok}");
        }
        for bad in ["", "-oProxyCommand=touch x", "-p", "a b", "a\nb", "--"] {
            assert!(!valid_host(bad), "{bad:?}");
        }
    }
}
