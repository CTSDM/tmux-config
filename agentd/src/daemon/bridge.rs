//! The desktop bridge: notifications and sounds of a tmux server on another
//! host (a server over ssh, no desktop of its own) shown and played on this
//! one, clicks sent back.
//!
//! `agentd bridge` runs on the desktop and listens on
//! `$XDG_RUNTIME_DIR/tmux-agents/desktop.sock`; ssh forwards it to the
//! server's `$XDG_RUNTIME_DIR/agentd-bridge.sock` (`RemoteForward`: in the
//! runtime folder itself, which exists at login, so sshd can bind it before
//! any agentd ran). A daemon connects there as soon as the socket appears and
//! sends its notifications and sounds over it instead of D-Bus and the
//! player; one that never saw it (no ssh, or a desktop) works as before.
//!
//! One JSON object per line, at most [`MAX_LINE`] bytes.
//! Daemon → desktop: `hello` (once: version, who it is), `sync` (its open
//! notifications' panes, after each hello), `notify`, `close`, `sound`,
//! `ping`. Desktop → daemon: `hello` (version), `pong`, `clicked`, `closed`
//! (dismissed or expired there).
//!
//! What a server says is not trusted beyond its own notifications: names are
//! checked, text is cut, sounds only come from the desktop's sound folder at
//! most at full volume.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::spawn_local;
use tokio::time::{sleep, timeout};

use super::notify::{Notifier, Origin};
use super::sound;
use super::visibility::{self, Seams};
use crate::core::Urgency;
use crate::procfs;

/// What a click (or a dismissal) does with its pane.
pub type OnClick = Rc<dyn Fn(&str)>;

/// The protocol's version, in both hellos. A peer without one is 1: no
/// heartbeat, no `sync` or `closed`.
const VERSION: u64 = 2;
/// The daemon's end, forwarded by ssh, in the runtime folder itself.
pub const REMOTE: &str = "agentd-bridge.sock";
/// The desktop's end.
pub const DESKTOP: &str = "desktop.sock";
/// A longer line ends the connection: nothing we send comes near it.
const MAX_LINE: usize = 16 << 10;
/// As the notifications' texts (contract I3).
const MAX_TEXT: usize = 300;
/// A line that can't be written in this long goes nowhere: a stuck bridge
/// must not hold the pane's effects.
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
/// A connection that doesn't say who it is in this long is dropped.
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the daemon looks for the socket while it has no bridge, and
/// checks the one it has.
const TICK: Duration = Duration::from_secs(5);
/// A ping this often; a v2 desktop not heard from in [`DEAD`] is gone (an
/// ssh that died with the laptop's sleep, still open on the server).
const PING: Duration = Duration::from_secs(20);
const DEAD: Duration = Duration::from_secs(60);
/// As `ag_raise_client`: the peer's process and 7 ancestors.
const ANCESTORS: usize = 8;

/// One connection to the desktop, as the daemon holds it.
struct Conn {
    writer: OwnedWriteHalf,
    peer: Rc<Heard>,
    pinged: Instant,
}

/// What the daemon heard from the desktop on one connection.
struct Heard {
    closed: Cell<bool>,
    at: Cell<Instant>,
    version: Cell<u64>,
}

/// The daemon's end. It connects as soon as the socket appears and again
/// after it breaks; once it has had a bridge, this host's notifications
/// belong to it, and the ones still open are shown again on reconnecting.
pub struct Link {
    path: PathBuf,
    /// `host/server`, which the desktop keys its notifications by.
    from: String,
    /// Who runs it, shown with the host.
    user: Option<String>,
    conn: Mutex<Option<Conn>>,
    on_click: RefCell<Option<OnClick>>,
    on_closed: RefCell<Option<OnClick>>,
    /// The open notifications as last sent, by pane.
    open: Rc<RefCell<BTreeMap<String, Value>>>,
    /// It had a bridge: notifications wait for it instead of going to a
    /// D-Bus a server doesn't have.
    used: Cell<bool>,
    tick: Duration,
    ping: Duration,
    dead: Duration,
}

impl Link {
    pub fn new(path: PathBuf, from: String) -> Link {
        let user = ["USER", "LOGNAME"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|u| !u.is_empty()));
        Link {
            path,
            from,
            user,
            conn: Mutex::new(None),
            on_click: RefCell::new(None),
            on_closed: RefCell::new(None),
            open: Rc::default(),
            used: Cell::new(false),
            tick: TICK,
            ping: PING,
            dead: DEAD,
        }
    }

    /// Where a clicked pane goes (the notifier's jump).
    pub fn on_click(&self, f: OnClick) {
        *self.on_click.borrow_mut() = Some(f);
    }

    /// Where a pane goes whose notification the desktop dismissed.
    pub fn on_closed(&self, f: OnClick) {
        *self.on_closed.borrow_mut() = Some(f);
    }

    /// Sends one message; `false` when there is no bridge to take it, and
    /// the caller does it here instead. Once there was one, a notification
    /// waits for it to come back, and a sound is dropped.
    pub async fn send(&self, message: &Value) -> bool {
        if let Some(pane) = message["close"]["pane"].as_str() {
            self.open.borrow_mut().remove(pane);
        }
        let mut conn = self.conn.lock().await;
        if conn.as_ref().is_some_and(|c| self.gone(c)) {
            *conn = None;
        }
        if conn.is_none() {
            *conn = self.connect().await;
        }
        let sent = match conn.as_mut() {
            Some(c) => write(&mut c.writer, message).await,
            None => false,
        };
        if !sent {
            *conn = None;
        }
        let taken = sent || self.used.get();
        // After connecting, which shows the ones already open.
        if let (true, Some(pane)) = (taken, message["notify"]["pane"].as_str()) {
            self.open
                .borrow_mut()
                .insert(pane.to_string(), message.clone());
        }
        taken
    }

    /// Keeps the bridge: connects when the socket appears, pings it, and
    /// drops it when the desktop stops answering. Until the daemon ends.
    pub async fn keep(self: Rc<Self>) {
        loop {
            sleep(self.tick).await;
            let mut conn = self.conn.lock().await;
            if conn.as_ref().is_some_and(|c| self.gone(c)) {
                *conn = None;
            }
            match conn.as_mut() {
                None => *conn = self.connect().await,
                Some(c) if c.pinged.elapsed() >= self.ping => {
                    c.pinged = Instant::now();
                    if !write(&mut c.writer, &json!({"ping": {}})).await {
                        *conn = None;
                    }
                }
                Some(_) => {}
            }
        }
    }

    /// The connection ended, or a desktop that answers pings stopped.
    fn gone(&self, c: &Conn) -> bool {
        c.peer.closed.get() || (c.peer.version.get() >= 2 && c.peer.at.get().elapsed() > self.dead)
    }

    async fn connect(&self) -> Option<Conn> {
        // Only a socket; what it was, to remove it only if it still is.
        let before = fs::symlink_metadata(&self.path).ok()?;
        if !before.file_type().is_socket() {
            return None;
        }
        let stream = match UnixStream::connect(&self.path).await {
            Ok(s) => s,
            Err(e) => {
                // sshd's default (StreamLocalBindUnlink no) leaves the socket
                // of a closed session behind, and the next ssh can't bind it.
                // Unless a new ssh has just bound it again.
                if e.kind() == io::ErrorKind::ConnectionRefused
                    && fs::symlink_metadata(&self.path)
                        .is_ok_and(|now| (now.dev(), now.ino()) == (before.dev(), before.ino()))
                {
                    let _ = fs::remove_file(&self.path);
                }
                return None;
            }
        };
        // Our own ssh's: in a shared /tmp, another user's socket gets nothing.
        let me = rustix::process::getuid().as_raw();
        if stream.peer_cred().ok()?.uid() != me {
            return None;
        }
        let (reader, mut writer) = stream.into_split();
        let open: Vec<Value> = self.open.borrow().values().cloned().collect();
        let panes: Vec<&str> = open
            .iter()
            .filter_map(|m| m["notify"]["pane"].as_str())
            .collect();
        let hello = json!({"hello": {"v": VERSION, "from": self.from, "user": self.user}});
        let sync = json!({"sync": {"panes": panes}});
        for m in [&hello, &sync].into_iter().chain(&open) {
            if !write(&mut writer, m).await {
                return None;
            }
        }
        self.used.set(true);
        let peer = Rc::new(Heard {
            closed: Cell::new(false),
            at: Cell::new(Instant::now()),
            version: Cell::new(1),
        });
        spawn_local(listen(
            reader,
            peer.clone(),
            self.open.clone(),
            self.on_click.borrow().clone(),
            self.on_closed.borrow().clone(),
        ));
        Some(Conn {
            writer,
            peer,
            pinged: Instant::now(),
        })
    }
}

/// One message, as one line, in time.
async fn write(writer: &mut OwnedWriteHalf, message: &Value) -> bool {
    let line = format!("{message}\n");
    matches!(
        timeout(WRITE_TIMEOUT, writer.write_all(line.as_bytes())).await,
        Ok(Ok(()))
    )
}

/// The next line as JSON (`Null` when it isn't); `None` at the end, or when
/// a line is longer than [`MAX_LINE`].
async fn next<R: AsyncBufRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> Option<Value> {
    buf.clear();
    let n = (&mut *reader)
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', buf)
        .await
        .ok()?;
    if n == 0 || n > MAX_LINE {
        return None;
    }
    Some(serde_json::from_slice(buf).unwrap_or(Value::Null))
}

/// What the desktop says, until the bridge goes.
async fn listen(
    reader: OwnedReadHalf,
    peer: Rc<Heard>,
    open: Rc<RefCell<BTreeMap<String, Value>>>,
    on_click: Option<OnClick>,
    on_closed: Option<OnClick>,
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    while let Some(v) = next(&mut reader, &mut buf).await {
        peer.at.set(Instant::now());
        if let Some(n) = v["hello"]["v"].as_u64() {
            peer.version.set(n);
        } else if let Some(pane) = v["clicked"]["pane"].as_str() {
            open.borrow_mut().remove(pane);
            if let Some(f) = &on_click {
                f(pane);
            }
        } else if let Some(pane) = v["closed"]["pane"].as_str() {
            open.borrow_mut().remove(pane);
            if let Some(f) = &on_closed {
                f(pane);
            }
        }
    }
    peer.closed.set(true);
}

fn urgency(s: &str) -> Urgency {
    match s {
        "low" => Urgency::Low,
        "critical" => Urgency::Critical,
        _ => Urgency::Normal,
    }
}

/// A name a server may use: `max` bytes of letters, digits and `._-`, plus
/// `extra`.
fn name_ok(s: &str, max: usize, extra: &[u8]) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b) || extra.contains(&b))
}

/// A tmux pane id, `%<n>`.
fn pane_ok(s: &str) -> bool {
    s.len() > 1 && s.len() <= 12 && s.starts_with('%') && s[1..].bytes().all(|b| b.is_ascii_digit())
}

/// A server's text for a notification: no control characters, cut.
fn text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TEXT)
        .collect()
}

/// A server's volume, at most full; empty (the default) when it isn't one.
fn volume(s: &str) -> String {
    match s.trim().parse::<f64>() {
        Ok(v) if v.is_finite() => v.clamp(0.0, 1.0).to_string(),
        _ => String::new(),
    }
}

/// A daemon connected to the desktop: its connection, where its messages
/// go, and the ssh process that carries it (its terminal window is raised on
/// a click).
struct Peer {
    conn: u64,
    tx: mpsc::UnboundedSender<String>,
    pid: Option<i32>,
}

type Peers = Rc<RefCell<HashMap<String, Peer>>>;

/// A message to the daemon that `key` (`<peer> <pane>`) belongs to.
fn route(peers: &Peers, key: &str, message: Value) -> Option<Option<i32>> {
    let (peer, _) = key.rsplit_once(' ')?;
    let peers = peers.borrow();
    let p = peers.get(peer)?;
    let _ = p.tx.send(format!("{message}\n"));
    Some(p.pid)
}

/// `agentd bridge`: until killed.
pub async fn serve(runtime: PathBuf) -> io::Result<()> {
    let path = runtime.join(DESKTOP);
    fs::create_dir_all(&runtime)?;
    if UnixStream::connect(&path).await.is_ok() {
        eprintln!("agentd bridge: already running on {}", path.display());
        return Ok(());
    }
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    eprintln!("agentd bridge: listening on {}", path.display());

    let peers: Peers = Rc::default();
    let seams = Rc::new(Seams::from_env());
    let mut notifier = Notifier::new(None, BTreeMap::new(), Rc::new(Notify::new()), None);
    // Keys are `<peer> <pane>`: neither has a space.
    let routes = peers.clone();
    notifier.on_click(Rc::new(move |key: &str| {
        let pane = key.rsplit_once(' ').map_or("", |(_, p)| p);
        if let Some(Some(pid)) = route(&routes, key, json!({"clicked": {"pane": pane}})) {
            let seams = seams.clone();
            spawn_local(async move { raise(&seams, pid).await });
        }
    }));
    let routes = peers.clone();
    notifier.on_closed(Rc::new(move |key: &str| {
        let pane = key.rsplit_once(' ').map_or("", |(_, p)| p);
        route(&routes, key, json!({"closed": {"pane": pane}}));
    }));
    let notifier = Rc::new(notifier);
    let sounds = Rc::new(sound::Config::from_env(runtime.clone()));
    let mut next = 0u64;
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            // Out of descriptors, say: wait, and keep serving the others.
            Err(e) => {
                crate::debug::log(&format!("bridge accept: {e}"));
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        next += 1;
        spawn_local(connection(
            stream,
            next,
            peers.clone(),
            notifier.clone(),
            sounds.clone(),
        ));
    }
}

/// One daemon, from its hello until it goes.
async fn connection(
    stream: UnixStream,
    conn: u64,
    peers: Peers,
    notifier: Rc<Notifier>,
    sounds: Rc<sound::Config>,
) {
    let pid = stream.peer_cred().ok().and_then(|c| c.pid());
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    let Ok(Some(hello)) = timeout(HELLO_TIMEOUT, next(&mut reader, &mut buf)).await else {
        return;
    };
    let Some(from) = hello["hello"]["from"]
        .as_str()
        .filter(|f| name_ok(f, 128, b"/"))
        .map(str::to_string)
    else {
        return;
    };
    let user = hello["hello"]["user"]
        .as_str()
        .filter(|u| name_ok(u, 32, b""))
        .map(str::to_string);
    // Two daemons of one host (tmux servers, or users) never share keys.
    let peer = format!("{}@{from}", user.as_deref().unwrap_or("-"));
    if !write(&mut writer, &json!({"hello": {"v": VERSION}})).await {
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    spawn_local(async move {
        while let Some(line) = rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let reply = tx.clone();
    // A daemon that reconnects replaces its old connection.
    peers
        .borrow_mut()
        .insert(peer.clone(), Peer { conn, tx, pid });
    let origin = Origin {
        host: host(&from),
        user: user.as_deref(),
    };
    while let Some(v) = next(&mut reader, &mut buf).await {
        let field = |m: &Value, k: &str| m[k].as_str().unwrap_or("").to_string();
        if let Some(m) = v.get("notify") {
            let pane = field(m, "pane");
            if !pane_ok(&pane) {
                continue;
            }
            notifier
                .show_remote(
                    origin,
                    &format!("{peer} {pane}"),
                    urgency(&field(m, "urgency")),
                    &text(&field(m, "title")),
                    &text(&field(m, "body")),
                )
                .await;
        } else if let Some(m) = v.get("close") {
            notifier
                .close(&format!("{peer} {}", field(m, "pane")))
                .await;
        } else if let Some(m) = v.get("sound") {
            let name = field(m, "name");
            sound::play(&sounds, &name, true, &volume(&field(m, "volume"))).await;
        } else if v.get("ping").is_some() {
            let _ = reply.send(format!("{}\n", json!({"pong": {}})));
        } else if let Some(panes) = v["sync"]["panes"].as_array() {
            // Closed on the server while the bridge was away.
            let keep: Vec<&str> = panes.iter().filter_map(Value::as_str).collect();
            let prefix = format!("{peer} ");
            let stale: Vec<String> = notifier
                .ids()
                .into_keys()
                .filter(|k| k.strip_prefix(&prefix).is_some_and(|p| !keep.contains(&p)))
                .collect();
            for key in stale {
                notifier.close(&key).await;
            }
        }
    }
    // Its notifications stay: after a reconnect it can still close them.
    let mut peers = peers.borrow_mut();
    if peers.get(&peer).is_some_and(|p| p.conn == conn) {
        peers.remove(&peer);
    }
}

/// The host of a daemon's `host/server`, what its notifications say they
/// come from.
fn host(from: &str) -> &str {
    from.split_once('/').map_or(from, |(h, _)| h)
}

/// Raises the terminal window the ssh process `pid` runs in (Hyprland only),
/// as `ag_raise_client` does for a local client.
async fn raise(seams: &Seams, pid: i32) {
    let Some(reply) = visibility::hypr(seams, "j/clients").await else {
        return;
    };
    let Ok(Value::Array(windows)) = serde_json::from_slice::<Value>(&reply) else {
        return;
    };
    let pids: Vec<i64> = windows.iter().filter_map(|w| w["pid"].as_i64()).collect();
    let mut pid = pid as u32;
    for _ in 0..ANCESTORS {
        if pids.contains(&(pid as i64)) {
            let _ = visibility::hypr(seams, &format!("dispatch focuswindow pid:{pid}")).await;
            return;
        }
        match procfs::stat(pid) {
            Some(s) if s.ppid > 1 => pid = s.ppid,
            _ => return,
        }
    }
}

/// The daemon's end of the bridge, for its runtime folder
/// (`$XDG_RUNTIME_DIR/tmux-agents`): next to it, in `$XDG_RUNTIME_DIR`.
pub fn remote(runtime: &Path) -> PathBuf {
    runtime.parent().unwrap_or(runtime).join(REMOTE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::LocalSet;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentd-bridge-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A link that ticks fast, to the socket in `d`.
    fn link(d: &Path) -> Rc<Link> {
        let mut l = Link::new(d.join(REMOTE), "host/default".into());
        l.tick = Duration::from_millis(10);
        l.ping = Duration::from_millis(20);
        l.dead = Duration::from_millis(100);
        Rc::new(l)
    }

    /// Reads lines as the desktop would.
    struct Desk {
        reader: BufReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
        buf: Vec<u8>,
    }

    impl Desk {
        async fn accept(listener: &UnixListener) -> Desk {
            let (s, _) = timeout(Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let (r, w) = s.into_split();
            Desk {
                reader: BufReader::new(r),
                writer: w,
                buf: Vec::new(),
            }
        }
        async fn line(&mut self) -> Value {
            timeout(
                Duration::from_secs(2),
                next(&mut self.reader, &mut self.buf),
            )
            .await
            .unwrap()
            .unwrap()
        }
        async fn say(&mut self, v: Value) {
            assert!(write(&mut self.writer, &v).await);
        }
    }

    async fn until(f: impl Fn() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out");
    }

    #[test]
    fn host_of_from_and_remote_path() {
        assert_eq!(host("box/agents"), "box");
        assert_eq!(host("box"), "box");
        assert_eq!(
            remote(Path::new("/run/user/7/tmux-agents")),
            PathBuf::from("/run/user/7/agentd-bridge.sock")
        );
    }

    #[test]
    fn what_a_server_may_say() {
        assert!(name_ok("box-1.lan/agents", 128, b"/"));
        assert!(!name_ok("box agents", 128, b"/"));
        assert!(!name_ok("../x", 128, b""));
        assert!(!name_ok("", 32, b""));
        assert!(pane_ok("%12"));
        assert!(!pane_ok("%"));
        assert!(!pane_ok("%1 x"));
        assert_eq!(text("a\x1b[1mb\nc"), "a [1mb c");
        assert_eq!(text(&"é".repeat(400)).chars().count(), MAX_TEXT);
        assert_eq!(volume("0.6"), "0.6");
        assert_eq!(volume("50"), "1");
        assert_eq!(volume("-2"), "0");
        assert_eq!(volume("NaN"), "");
        assert_eq!(volume("loud"), "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_line_too_long_ends_it() {
        let data = format!("{{\"a\":1}}\n{}\n", "x".repeat(MAX_LINE + 10));
        let mut r = BufReader::new(data.as_bytes());
        let mut buf = Vec::new();
        assert_eq!(next(&mut r, &mut buf).await, Some(json!({"a": 1})));
        assert_eq!(next(&mut r, &mut buf).await, None);
        let mut r = BufReader::new("not json\n".as_bytes());
        assert_eq!(next(&mut r, &mut buf).await, Some(Value::Null));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_bridge_sends_nothing_and_only_a_stale_socket_goes() {
        let d = dir("stale");
        LocalSet::new()
            .run_until(async {
                let link = link(&d);
                assert!(!link.send(&json!({"x": 1})).await, "no socket");
                // Not a socket: left alone.
                fs::write(d.join(REMOTE), "").unwrap();
                assert!(!link.send(&json!({"x": 1})).await);
                assert!(d.join(REMOTE).exists());
                fs::remove_file(d.join(REMOTE)).unwrap();
                // A socket nobody listens on, as ssh leaves it.
                drop(std::os::unix::net::UnixListener::bind(d.join(REMOTE)).unwrap());
                assert!(!link.send(&json!({"x": 1})).await);
                assert!(!d.join(REMOTE).exists(), "removed for the next ssh");
            })
            .await;
        fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hello_sync_messages_and_clicks_back() {
        let d = dir("link");
        LocalSet::new()
            .run_until(async {
                let listener = UnixListener::bind(d.join(REMOTE)).unwrap();
                let link = link(&d);
                let clicked: Rc<RefCell<Vec<String>>> = Rc::default();
                let got = clicked.clone();
                link.on_click(Rc::new(move |p: &str| got.borrow_mut().push(p.into())));
                let closed: Rc<RefCell<Vec<String>>> = Rc::default();
                let got = closed.clone();
                link.on_closed(Rc::new(move |p: &str| got.borrow_mut().push(p.into())));
                assert!(link.send(&json!({"notify": {"pane": "%1"}})).await);
                let mut desk = Desk::accept(&listener).await;
                let hello = desk.line().await;
                assert_eq!(hello["hello"]["from"], "host/default");
                assert_eq!(hello["hello"]["v"], VERSION);
                assert_eq!(desk.line().await["sync"]["panes"], json!([]));
                assert_eq!(desk.line().await["notify"]["pane"], "%1");
                desk.say(json!({"clicked": {"pane": "%1"}})).await;
                until(|| !clicked.borrow().is_empty()).await;
                assert_eq!(*clicked.borrow(), ["%1"]);
                assert!(link.open.borrow().is_empty(), "a click closes it");
                assert!(link.send(&json!({"notify": {"pane": "%2"}})).await);
                assert_eq!(desk.line().await["notify"]["pane"], "%2");
                desk.say(json!({"closed": {"pane": "%2"}})).await;
                until(|| !closed.borrow().is_empty()).await;
                assert!(link.open.borrow().is_empty(), "dismissed there");
            })
            .await;
        fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waits_while_the_bridge_is_away_then_shows_what_is_still_open() {
        let d = dir("away");
        LocalSet::new()
            .run_until(async {
                let listener = UnixListener::bind(d.join(REMOTE)).unwrap();
                let link = link(&d);
                assert!(link.send(&json!({"notify": {"pane": "%1"}})).await);
                let mut desk = Desk::accept(&listener).await;
                for _ in 0..3 {
                    desk.line().await;
                }
                // The ssh goes (the laptop sleeps).
                drop(desk);
                drop(listener);
                fs::remove_file(d.join(REMOTE)).unwrap();
                sleep(Duration::from_millis(20)).await;
                assert!(
                    link.send(&json!({"notify": {"pane": "%2"}})).await,
                    "kept for the bridge, not D-Bus"
                );
                assert!(link.send(&json!({"close": {"pane": "%1"}})).await);
                assert!(link.send(&json!({"sound": {"name": "x"}})).await, "dropped");
                // It comes back; the daemon finds it by itself.
                let listener = UnixListener::bind(d.join(REMOTE)).unwrap();
                spawn_local(link.clone().keep());
                let mut desk = Desk::accept(&listener).await;
                assert_eq!(desk.line().await["hello"]["v"], VERSION);
                assert_eq!(desk.line().await["sync"]["panes"], json!(["%2"]));
                assert_eq!(desk.line().await["notify"]["pane"], "%2");
            })
            .await;
        fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_desktop_that_stops_answering_is_dropped() {
        let d = dir("dead");
        LocalSet::new()
            .run_until(async {
                let listener = UnixListener::bind(d.join(REMOTE)).unwrap();
                let link = link(&d);
                spawn_local(link.clone().keep());
                let mut desk = Desk::accept(&listener).await;
                desk.line().await;
                desk.line().await;
                desk.say(json!({"hello": {"v": 2}})).await;
                // It answers pings for a while...
                for _ in 0..3 {
                    assert!(desk.line().await.get("ping").is_some());
                    desk.say(json!({"pong": {}})).await;
                }
                // ...then goes silent, the connection still open: a new one.
                let _silent = desk;
                let mut again = Desk::accept(&listener).await;
                assert_eq!(again.line().await["hello"]["v"], VERSION);
            })
            .await;
        fs::remove_dir_all(&d).unwrap();
    }
}
