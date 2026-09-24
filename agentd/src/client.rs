//! Talking to the daemon from `hook`, `ensure` and `ctl`: find its socket,
//! start it when it is not there, one request and one reply.

use std::env;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::identity::{self, Paths};
use crate::proto::{Reply, Request};

/// A Unix socket path must fit in `sun_path` (108 bytes with its NUL).
const MAX_SOCKET_PATH: usize = 107;

/// The daemon's files for the tmux server in `$TMUX`. `None` when its socket
/// path would be too long to bind: then no daemon can start, and hooks must
/// not wait for one.
pub fn paths(tmux: &OsStr) -> Option<Paths> {
    let socket = identity::server_socket(tmux)?;
    let dir = identity::runtime_dir(env::var_os("XDG_RUNTIME_DIR").as_deref());
    let paths = Paths::new(&dir, &identity::server_id(socket));
    (paths.socket.as_os_str().len() <= MAX_SOCKET_PATH).then_some(paths)
}

/// Starts `agentd daemon` in the background, detached from our stdio (the
/// hook's stdout is the agent's pipe). The daemon makes itself a session
/// leader, and a second one exits at once (lock). Never while agentd is
/// switched off.
pub fn start_daemon() -> io::Result<()> {
    if identity::off() {
        return Err(io::Error::other("agentd.off"));
    }
    Command::new(env::current_exe()?)
        .arg("daemon")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

/// Connects, starting the daemon if nobody answers, and keeps trying for
/// `budget`.
pub fn connect_or_start(paths: &Paths, budget: Duration) -> Option<UnixStream> {
    if let Ok(stream) = UnixStream::connect(&paths.socket) {
        return Some(stream);
    }
    let deadline = Instant::now() + budget;
    start_daemon().ok()?;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
        if let Ok(stream) = UnixStream::connect(&paths.socket) {
            return Some(stream);
        }
    }
    None
}

/// Waits up to `budget` for the daemon to exit: its lock is free.
pub fn wait_gone(paths: &Paths, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let free = File::open(&paths.lock).is_ok_and(|f| {
            rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockShared).is_ok()
        });
        if free {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Sends one request and waits up to `timeout` for the reply.
pub fn call(mut stream: UnixStream, request: &Request, timeout: Duration) -> io::Result<Reply> {
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    serde_json::from_str(&reply).map_err(io::Error::other)
}
