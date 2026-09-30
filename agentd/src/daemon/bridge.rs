//! The desktop bridge: notifications and sounds of a tmux server on another
//! host (a server over ssh, no desktop of its own) shown and played on this
//! one, clicks sent back.
//!
//! `agentd bridge` runs on the desktop and listens on
//! `$XDG_RUNTIME_DIR/tmux-agents/desktop.sock`; ssh forwards it to the
//! server's `$XDG_RUNTIME_DIR/tmux-agents/bridge.sock`
//! (`RemoteForward`). A daemon that can connect there sends its
//! notifications and sounds over it instead of D-Bus and the player; one
//! that can't (no ssh, or a desktop) works as before.
//!
//! One JSON object per line. Daemon → desktop: `hello` (once, who it is),
//! `notify`, `close`, `sound`. Desktop → daemon: `clicked`.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::spawn_local;
use tokio::time::timeout;

use super::notify::{Notifier, Origin};
use super::sound;
use super::visibility::{self, Seams};
use crate::core::Urgency;
use crate::procfs;

/// What a click does with its pane.
pub type OnClick = Rc<dyn Fn(&str)>;

/// The daemon's end, forwarded by ssh.
pub const REMOTE: &str = "bridge.sock";
/// The desktop's end.
pub const DESKTOP: &str = "desktop.sock";
/// A line that can't be written in this long goes nowhere: a stuck bridge
/// must not hold the pane's effects.
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
/// As `ag_raise_client`: the peer's process and 7 ancestors.
const ANCESTORS: usize = 8;

/// The daemon's end: connected on first use and again after it breaks.
pub struct Link {
    path: PathBuf,
    /// `host/server`, which the desktop keys its notifications by.
    from: String,
    /// Who runs it, shown with the host.
    user: Option<String>,
    conn: Mutex<Option<(OwnedWriteHalf, Rc<Cell<bool>>)>>,
    on_click: RefCell<Option<OnClick>>,
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
        }
    }

    /// Where a clicked pane goes (the notifier's jump).
    pub fn on_click(&self, f: OnClick) {
        *self.on_click.borrow_mut() = Some(f);
    }

    /// Sends one message; `false` when there is no bridge to take it, and
    /// the caller does it here instead.
    pub async fn send(&self, message: &Value) -> bool {
        let mut conn = self.conn.lock().await;
        if conn.as_ref().is_some_and(|(_, closed)| closed.get()) {
            *conn = None;
        }
        if conn.is_none() {
            *conn = self.connect().await;
        }
        let Some((writer, _)) = conn.as_mut() else {
            return false;
        };
        let line = format!("{message}\n");
        match timeout(WRITE_TIMEOUT, writer.write_all(line.as_bytes())).await {
            Ok(Ok(())) => true,
            _ => {
                *conn = None;
                false
            }
        }
    }

    async fn connect(&self) -> Option<(OwnedWriteHalf, Rc<Cell<bool>>)> {
        let stream = match UnixStream::connect(&self.path).await {
            Ok(s) => s,
            Err(e) => {
                // sshd's default (StreamLocalBindUnlink no) leaves the socket
                // of a closed session behind, and the next ssh can't bind it.
                if e.kind() == io::ErrorKind::ConnectionRefused {
                    let _ = fs::remove_file(&self.path);
                }
                return None;
            }
        };
        let (reader, mut writer) = stream.into_split();
        let hello = format!(
            "{}\n",
            json!({"hello": {"from": self.from, "user": self.user}})
        );
        timeout(WRITE_TIMEOUT, writer.write_all(hello.as_bytes()))
            .await
            .ok()?
            .ok()?;
        let closed = Rc::new(Cell::new(false));
        let on_click = self.on_click.borrow().clone();
        spawn_local(clicks(reader, closed.clone(), on_click));
        Some((writer, closed))
    }
}

/// The desktop's clicks, until the bridge goes.
async fn clicks(reader: OwnedReadHalf, closed: Rc<Cell<bool>>, on_click: Option<OnClick>) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let (Some(pane), Some(f)) = (v["clicked"]["pane"].as_str(), &on_click) {
            f(pane);
        }
    }
    closed.set(true);
}

fn urgency(s: &str) -> Urgency {
    match s {
        "low" => Urgency::Low,
        "critical" => Urgency::Critical,
        _ => Urgency::Normal,
    }
}

/// A daemon connected to the desktop: where its clicks go, and the ssh
/// process that carries it (its terminal window is raised on a click).
struct Peer {
    conn: u64,
    clicks: mpsc::UnboundedSender<String>,
    pid: Option<i32>,
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

    let peers: Rc<RefCell<HashMap<String, Peer>>> = Rc::default();
    let seams = Rc::new(Seams::from_env());
    let mut notifier = Notifier::new(None, BTreeMap::new(), Rc::new(Notify::new()), None);
    let routes = peers.clone();
    let raise_seams = seams.clone();
    // Keys are `<from> <pane>`; a pane id has no space.
    notifier.on_click(Rc::new(move |key: &str| {
        let Some((from, pane)) = key.rsplit_once(' ') else {
            return;
        };
        let routes = routes.borrow();
        let Some(peer) = routes.get(from) else {
            return;
        };
        let _ = peer
            .clicks
            .send(format!("{}\n", json!({"clicked": {"pane": pane}})));
        if let Some(pid) = peer.pid {
            let seams = raise_seams.clone();
            spawn_local(async move { raise(&seams, pid).await });
        }
    }));
    let notifier = Rc::new(notifier);
    let sounds = Rc::new(sound::Config::from_env(runtime.clone()));
    let mut next = 0u64;
    loop {
        let (stream, _) = listener.accept().await?;
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
    peers: Rc<RefCell<HashMap<String, Peer>>>,
    notifier: Rc<Notifier>,
    sounds: Rc<sound::Config>,
) {
    let pid = stream.peer_cred().ok().and_then(|c| c.pid());
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let Ok(Some(first)) = lines.next_line().await else {
        return;
    };
    let Ok(hello) = serde_json::from_str::<Value>(&first) else {
        return;
    };
    let Some(from) = hello["hello"]["from"].as_str().map(str::to_string) else {
        return;
    };
    let user = hello["hello"]["user"].as_str().map(str::to_string);
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    spawn_local(async move {
        while let Some(line) = rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    // A daemon that reconnects replaces its old connection.
    peers.borrow_mut().insert(
        from.clone(),
        Peer {
            conn,
            clicks: tx,
            pid,
        },
    );
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let text = |m: &Value, k: &str| m[k].as_str().unwrap_or("").to_string();
        if let Some(m) = v.get("notify") {
            let key = format!("{from} {}", text(m, "pane"));
            let level = urgency(&text(m, "urgency"));
            notifier
                .show_remote(
                    Origin {
                        host: host(&from),
                        user: user.as_deref(),
                    },
                    &key,
                    level,
                    &text(m, "title"),
                    &text(m, "body"),
                )
                .await;
        } else if let Some(m) = v.get("close") {
            notifier.close(&format!("{from} {}", text(m, "pane"))).await;
        } else if let Some(m) = v.get("sound") {
            sound::play(&sounds, &text(m, "name"), true, &text(m, "volume")).await;
        }
    }
    // Its notifications stay: after a reconnect it can still close them.
    let mut peers = peers.borrow_mut();
    if peers.get(&from).is_some_and(|p| p.conn == conn) {
        peers.remove(&from);
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

/// The daemon's end of the bridge, for its runtime folder.
pub fn remote(runtime: &Path) -> PathBuf {
    runtime.join(REMOTE)
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

    #[test]
    fn host_of_from() {
        assert_eq!(host("box/agents"), "box");
        assert_eq!(host("box"), "box");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_bridge_sends_nothing_and_a_stale_socket_goes() {
        let d = dir("stale");
        LocalSet::new()
            .run_until(async {
                let link = Link::new(remote(&d), "h/s".into());
                assert!(!link.send(&json!({"x": 1})).await, "no socket");
                // A socket nobody listens on, as ssh leaves it.
                drop(std::os::unix::net::UnixListener::bind(remote(&d)).unwrap());
                assert!(remote(&d).exists());
                assert!(!link.send(&json!({"x": 1})).await);
                assert!(!remote(&d).exists(), "removed for the next ssh");
            })
            .await;
        fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hello_messages_and_clicks_back() {
        let d = dir("link");
        LocalSet::new()
            .run_until(async {
                let listener = UnixListener::bind(remote(&d)).unwrap();
                let link = Link::new(remote(&d), "host/default".into());
                let clicked: Rc<RefCell<Vec<String>>> = Rc::default();
                let got = clicked.clone();
                link.on_click(Rc::new(move |p: &str| got.borrow_mut().push(p.into())));
                assert!(link.send(&json!({"close": {"pane": "%1"}})).await);
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                let hello: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(hello["hello"]["from"], "host/default");
                let close: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(close["close"]["pane"], "%1");
                w.write_all(b"{\"clicked\":{\"pane\":\"%1\"}}\n")
                    .await
                    .unwrap();
                for _ in 0..100 {
                    if !clicked.borrow().is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert_eq!(*clicked.borrow(), ["%1"]);
                // The bridge goes: the next message connects again, and finds nobody.
                drop(w);
                drop(lines);
                drop(listener);
                let _ = fs::remove_file(remote(&d));
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert!(!link.send(&json!({"x": 1})).await);
            })
            .await;
        fs::remove_dir_all(&d).unwrap();
    }
}
