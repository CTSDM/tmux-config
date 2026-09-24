//! N1-N3 over D-Bus, against a private bus: a `dbus-daemon` of our own with
//! nothing activatable (no service folders: the desktop's notification
//! daemon can't be started through it), started without the desktop's
//! variables, and a fake notification server that owns the name before
//! agentd says anything. agentd sees only that bus. Nothing reaches the
//! desktop.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};
use std::{env, fs};

use agentd::identity::{self, Paths};
use serde_json::{Value, json};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

const PATH: &str = "/org/freedesktop/Notifications";

/// What the fake server was asked.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Notify {
        app: String,
        replaces: u32,
        icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        urgency: Option<u8>,
        timeout: i32,
    },
    Close(u32),
}

struct Fake {
    calls: Arc<Mutex<Vec<Call>>>,
    next: u32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Fake {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &mut self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        self.calls.lock().unwrap().push(Call::Notify {
            app: app_name,
            replaces: replaces_id,
            icon: app_icon,
            summary,
            body,
            actions,
            urgency: hints.get("urgency").and_then(|v| u8::try_from(v).ok()),
            timeout: expire_timeout,
        });
        if replaces_id != 0 {
            return replaces_id;
        }
        self.next += 1;
        self.next
    }

    async fn close_notification(&self, id: u32) {
        self.calls.lock().unwrap().push(Call::Close(id));
    }

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

/// The private bus and the fake server, on a thread of their own.
struct Bus {
    dbus: Child,
    dir: PathBuf,
    address: String,
    calls: Arc<Mutex<Vec<Call>>>,
    click: mpsc::Sender<u32>,
}

impl Bus {
    fn start(dir: &Path) -> Option<Bus> {
        if Command::new("dbus-daemon")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("dbus-daemon not found: skipped");
            return None;
        }
        let socket = dir.join("bus");
        let config = dir.join("bus.conf");
        fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                socket.display()
            ),
        )
        .unwrap();
        // A clean environment: nothing of the desktop's session.
        let dbus = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .arg("--nofork")
            .env_clear()
            .env("PATH", env::var("PATH").unwrap_or_default())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let address = format!("unix:path={}", socket.display());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (click, clicks) = mpsc::channel::<u32>();
        // From here on a panic still kills the bus (Drop).
        let bus = Bus {
            dbus,
            dir: dir.to_path_buf(),
            address: address.clone(),
            calls: calls.clone(),
            click,
        };
        wait_for("the private bus", || UnixStream::connect(&socket).is_ok());

        let (ready, owned) = mpsc::channel::<()>();
        let (server_calls, server_address) = (calls.clone(), address.clone());
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let fake = Fake {
                    calls: server_calls,
                    next: 0,
                };
                let build = zbus::connection::Builder::address(server_address.as_str())
                    .unwrap()
                    .name("org.freedesktop.Notifications")
                    .unwrap()
                    .serve_at(PATH, fake)
                    .unwrap()
                    .build();
                let conn = match tokio::time::timeout(Duration::from_secs(4), build).await {
                    Ok(Ok(conn)) => conn,
                    Ok(Err(e)) => panic!("fake server: {e}"),
                    Err(_) => panic!("fake server: no connection within 4 s"),
                };
                ready.send(()).unwrap();
                let emitter = SignalEmitter::new(&conn, PATH).unwrap();
                loop {
                    match clicks.try_recv() {
                        Ok(id) => Fake::action_invoked(&emitter, id, "default").await.unwrap(),
                        Err(mpsc::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(10)).await
                        }
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            });
        });
        // The name is ours before agentd says anything.
        owned
            .recv_timeout(Duration::from_secs(5))
            .expect("the fake server owns the name");
        Some(bus)
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn wait_calls(&self, n: usize) -> Vec<Call> {
        wait_for(&format!("{n} calls"), || self.calls().len() >= n);
        self.calls()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.dbus.kill();
        let _ = self.dbus.wait();
        let _ = fs::remove_file(self.dir.join("bus"));
    }
}

struct Server {
    name: String,
    socket: String,
    pid: u32,
    runtime: PathBuf,
    paths: Paths,
    daemon: Option<Child>,
}

impl Server {
    fn start(runtime: &Path) -> Server {
        let name = format!("agentd-nt-{}", std::process::id());
        let mut s = Server {
            name,
            socket: String::new(),
            pid: 0,
            runtime: runtime.to_path_buf(),
            paths: Paths::new(runtime, "unset"),
            daemon: None,
        };
        s.tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            "80",
            "-y",
            "24",
            "sleep 600",
        ]);
        s.socket = s.tmux(&["display", "-p", "#{socket_path}"]);
        s.pid = s.tmux(&["display", "-p", "#{pid}"]).parse().unwrap();
        let dir = identity::runtime_dir(Some(runtime.as_os_str()));
        s.paths = Paths::new(&dir, &identity::server_id(Path::new(&s.socket)));
        s
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .arg("-L")
            .arg(&self.name)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    fn start_daemon(&mut self, bus: &Bus) {
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .arg("daemon")
            .env_clear()
            .env("PATH", env::var("PATH").unwrap_or_default())
            .env("HOME", &self.runtime)
            .env("TMUX", format!("{},{},0", self.socket, self.pid))
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_STATE_HOME", self.runtime.join("state"))
            .env("AG_FOCUS_CLIENT", "none")
            // No sound files: nothing plays.
            .env("AG_SOUNDS", self.runtime.join("no-sounds"))
            .env("AG_SOUND_PLAYER", "/bin/true")
            .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        wait_for("the daemon's socket", || {
            UnixStream::connect(&self.paths.socket).is_ok()
        });
    }

    fn stop_daemon(&mut self) {
        let mut child = self.daemon.take().unwrap();
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        child.wait().unwrap();
    }

    fn hook(&self, pane: &str, event: Value) {
        let pane_pid: u32 = self
            .tmux(&["display", "-p", "-t", pane, "#{pane_pid}"])
            .parse()
            .unwrap();
        let mut stream = UnixStream::connect(&self.paths.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = json!({"v": 1, "kind": "claude", "pane": pane, "event": event,
            "chain": [[pane_pid, "claude", 0]], "env": {}, "t": 0});
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert!(line.contains("\"ok\":true"), "{line}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(format!(
            "/tmp/tmux-{}/{}",
            rustix::process::getuid().as_raw(),
            self.name
        ));
    }
}

/// A folder removed when the test ends, whatever happens.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(Instant::now() < deadline, "{what}: not within 5 s");
        sleep(Duration::from_millis(20));
    }
}

fn ev(name: &str, fields: Value) -> Value {
    let mut e = json!({"hook_event_name": name, "session_id": "s1"});
    e.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    e
}

#[test]
fn n1_n3_over_a_private_bus() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let tmp = TempDir(PathBuf::from(format!(
        "/tmp/agentd-nt-{}",
        std::process::id()
    )));
    let dir = tmp.0.clone();
    fs::create_dir_all(dir.join("bin")).unwrap();
    let Some(bus) = Bus::start(&dir) else { return };
    let mut server = Server::start(&dir);
    // agent-jump, as a click runs it: a stand-in that records its pane.
    let jumped = dir.join("jumped");
    let jump = dir.join("bin/agent-jump");
    fs::write(
        &jump,
        format!("#!/bin/sh\necho \"$1\" >> {}\n", jumped.display()),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&jump).status().unwrap();
    server.tmux(&[
        "set",
        "-g",
        "@agents_bin",
        dir.join("bin").to_str().unwrap(),
    ]);
    server.tmux(&["set", "-g", "@agent_sound", "off"]);
    server.start_daemon(&bus);
    let pane = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    server.tmux(&["select-pane", "-t", &pane, "-T", "✳ Fix CSV"]);

    // Entering needs, away: a critical notification (N1, N2).
    server.hook(
        &pane,
        ev(
            "PermissionRequest",
            // As the hook sends it: the input already reduced to its detail (I3).
            json!({"tool_name": "Bash", "tool_use_id": "t1", "detail": "ls"}),
        ),
    );
    let calls = bus.wait_calls(1);
    assert_eq!(
        calls[0],
        Call::Notify {
            app: "tmux agents".into(),
            replaces: 0,
            icon: "utilities-terminal".into(),
            summary: "main · Fix CSV".into(),
            body: "Needs permission: Bash: ls".into(),
            actions: vec!["default".into(), "Open".into()],
            urgency: Some(2),
            timeout: -1,
        }
    );
    // Leaving needs closes it (N3).
    server.hook(
        &pane,
        ev(
            "PostToolUse",
            json!({"tool_name": "Bash", "tool_use_id": "t1"}),
        ),
    );
    assert_eq!(bus.wait_calls(2)[1], Call::Close(1));
    // Done: a normal one; a click on it runs agent-jump for the pane.
    server.hook(
        &pane,
        ev("Stop", json!({"last_assistant_message": "All done"})),
    );
    let calls = bus.wait_calls(3);
    assert!(
        matches!(&calls[2], Call::Notify { body, urgency: Some(1), replaces: 0, .. } if body == "All done"),
        "{calls:?}"
    );
    bus.click.send(2).unwrap();
    wait_for("agent-jump", || {
        fs::read_to_string(&jumped).is_ok_and(|j| j.trim() == pane)
    });

    // A question, then the daemon restarts: the new one closes it at SessionEnd.
    server.hook(&pane, ev("Elicitation", json!({})));
    assert!(matches!(
        &bus.wait_calls(4)[3],
        Call::Notify { replaces: 0, .. }
    ));
    server.stop_daemon();
    let saved: Value = serde_json::from_slice(&fs::read(&server.paths.state).unwrap()).unwrap();
    assert_eq!(saved["notifications"], json!({pane.clone(): 3}));
    server.start_daemon(&bus);
    server.hook(&pane, ev("SessionEnd", json!({})));
    assert_eq!(bus.wait_calls(5)[4], Call::Close(3));
    // No pane option carries it (C3).
    assert!(!server.tmux(&["show", "-p", "-t", &pane]).contains("notify"));

    drop(server);
    drop(bus);
    drop(tmp);
}
