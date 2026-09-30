//! `agentd remote <host> <name>`: the local end of a remote pane
//! (design.md, "Remote panes"). The terminal in raw mode, joined over ssh to
//! the holder of `<name>` on `<host>`; the held agents' events to the local
//! daemon as this pane's own (I6). It reconnects when ssh drops.

use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
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
/// Keys are only read while the holder is there: until then ssh may be
/// asking for a password on the same terminal.
const KEYS_POLL: Duration = Duration::from_millis(100);

pub fn run(host: &str, name: &str) -> ExitCode {
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
    let code = Session::new(host, name, stdin, pane.clone()).run();
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
    stdin: OwnedFd,
    pane: Pane,
    /// Where keys and sizes go: ssh's stdin, while connected.
    up: Arc<Mutex<Option<ChildStdin>>>,
    connected: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
}

impl Session {
    fn new(host: &str, name: &str, stdin: OwnedFd, pane: Pane) -> Session {
        Session {
            host: host.to_string(),
            name: name.to_string(),
            stdin,
            pane,
            up: Arc::default(),
            connected: Arc::default(),
            quit: Arc::default(),
        }
    }

    fn run(self) -> ExitCode {
        let Ok(cooked) = tcgetattr(&self.stdin) else {
            return ExitCode::FAILURE;
        };
        let Ok(stdout) = io::stdout().as_fd().try_clone_to_owned() else {
            return ExitCode::FAILURE;
        };
        let mut out = File::from(stdout);
        let (wake, woken) = mpsc::channel::<()>();
        self.keys();
        self.signals(wake);
        let events = self.events();
        let mut have: Option<u64> = None;
        let mut attached_before = false;
        let mut retry = RETRY_FIRST;
        loop {
            let end = match self.transport() {
                Ok(child) => self.connection(
                    child,
                    &cooked,
                    &mut out,
                    &events,
                    &mut have,
                    &mut attached_before,
                    &mut retry,
                ),
                Err(e) => {
                    eprint!("agentd remote: {e}\r\n");
                    End::Lost
                }
            };
            let _ = tcsetattr(&self.stdin, OptionalActions::Now, &cooked);
            match end {
                End::Exit(code) => return ExitCode::from(code.clamp(0, 255) as u8),
                End::Detached(why) => {
                    eprint!("\r\n[{} on {}: {why}]\r\n", self.name, self.host);
                    return ExitCode::SUCCESS;
                }
                End::Lost if self.quit.load(Ordering::Relaxed) => return ExitCode::SUCCESS,
                End::Lost => {}
            }
            eprint!(
                "\r\n[{} on {}: connection lost, trying again in {}s; ctrl-c gives up, it stays held there]\r\n",
                self.name,
                self.host,
                retry.as_secs()
            );
            match woken.recv_timeout(retry) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return ExitCode::SUCCESS,
            }
            retry = (retry * 2).min(RETRY_LAST);
        }
    }

    fn transport(&self) -> io::Result<Child> {
        hold_command(&self.host, Some(&self.name))?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
    }

    #[allow(clippy::too_many_arguments)]
    fn connection(
        &self,
        mut child: Child,
        cooked: &Termios,
        out: &mut File,
        events: &Sender<Vec<u8>>,
        have: &mut Option<u64>,
        attached_before: &mut bool,
        retry: &mut Duration,
    ) -> End {
        let (Some(mut up), Some(mut down)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return End::Lost;
        };
        let (rows, cols) = self.size();
        let hello = json!({
            "term": env::var("TERM").unwrap_or_default(),
            "rows": rows,
            "cols": cols,
            "have": *have,
        });
        let end = if frame::write(&mut up, HELLO, hello.to_string().as_bytes()).is_err() {
            End::Lost
        } else {
            *self.lock_up() = Some(up);
            loop {
                match frame::read(&mut down) {
                    Ok(Some((ATTACHED, p))) => {
                        let Ok(a) = serde_json::from_slice::<Attached>(&p) else {
                            break End::Lost;
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
                                "\r\n[{} on {}: a new shell, the old one is gone]\r\n",
                                self.name, self.host
                            );
                        }
                        *have = Some(a.at);
                        *attached_before = true;
                        *retry = RETRY_FIRST;
                        self.connected.store(true, Ordering::Relaxed);
                        // It may have changed since the hello.
                        self.resize();
                    }
                    Ok(Some((OUTPUT, p))) => {
                        if out.write_all(&p).is_err() {
                            break End::Lost;
                        }
                        *have = have.map(|h| h + p.len() as u64);
                    }
                    Ok(Some((EVENT, p))) => {
                        let _ = events.send(p);
                    }
                    Ok(Some((EXIT, p))) => {
                        let code = serde_json::from_slice::<serde_json::Value>(&p)
                            .ok()
                            .and_then(|v| v["code"].as_i64())
                            .unwrap_or(0);
                        break End::Exit(code as i32);
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
        self.connected.store(false, Ordering::Relaxed);
        *self.lock_up() = None;
        let _ = child.kill();
        let _ = child.wait();
        end
    }

    fn lock_up(&self) -> std::sync::MutexGuard<'_, Option<ChildStdin>> {
        self.up.lock().unwrap_or_else(|e| e.into_inner())
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

    /// Keys to the holder, read only while it is there.
    fn keys(&self) {
        let (Ok(stdin), up, connected, quit) = (
            self.stdin.try_clone(),
            self.up.clone(),
            self.connected.clone(),
            self.quit.clone(),
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
                if !connected.load(Ordering::Relaxed) {
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
                match rustix::io::read(&stdin, &mut buf) {
                    Ok(0) => return,
                    Ok(n) => send(&up, INPUT, &buf[..n]),
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
                    Err(_) => return,
                }
            }
        });
    }

    /// Window sizes to the holder; ctrl-c (the terminal is cooked while
    /// connecting), a hangup or a TERM give up.
    fn signals(&self, wake: Sender<()>) {
        let (up, connected, quit, stdin) = (
            self.up.clone(),
            self.connected.clone(),
            self.quit.clone(),
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
                            if connected.load(Ordering::Relaxed) {
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
                quit.store(true, Ordering::Relaxed);
                // ssh goes with the connection: closing its stdin ends it.
                *up.lock().unwrap_or_else(|e| e.into_inner()) = None;
                let _ = wake.send(());
            });
        });
    }

    /// The held agents' events to the local daemon, in order, as the pane's
    /// own (I6).
    fn events(&self) -> Sender<Vec<u8>> {
        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
        let Some((tmux, pane)) = self.pane.tmux.clone() else {
            return tx;
        };
        let host = self.host.clone();
        thread::spawn(move || {
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
                remote.host = host.clone();
                request.pane = pane.clone();
                request.chain = chain.clone();
                request.parked = None;
                if let Some(stream) = client::connect_or_start(&paths, START_BUDGET) {
                    let _ = client::call(stream, &Request::Hook(Box::new(request)), REPLY_TIMEOUT);
                }
            }
        });
        tx
    }
}

/// `agentd remote <host>`: the shells held there.
pub fn list(host: &str) -> ExitCode {
    match hold_command(host, None).and_then(|mut c| c.status()) {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("agentd remote: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `agentd hold [name]` on the host, over ssh; `-` runs it here.
fn hold_command(host: &str, name: Option<&str>) -> io::Result<Command> {
    if host == "-" {
        let mut c = Command::new(env::current_exe()?);
        c.arg("hold").args(name);
        return Ok(c);
    }
    let var = |n| env::var_os(n).filter(|v| !v.is_empty());
    let agentd = var("AGENTD_REMOTE_AGENTD").map_or_else(
        || "agentd".to_string(),
        |v| v.to_string_lossy().into_owned(),
    );
    let mut c = Command::new(var("AGENTD_SSH").unwrap_or_else(|| "ssh".into()));
    // No forwards: the desktop bridge's RemoteForward belongs to the
    // interactive ssh, and a second one would only fail to bind.
    c.args([
        "-T",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=3",
        host,
    ]);
    // A non-login shell runs it: setup.sh's place may not be in its PATH.
    c.arg(format!(
        "PATH=\"$HOME/.local/bin:$PATH\" exec {agentd} hold {}",
        name.unwrap_or("")
    ));
    Ok(c)
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
