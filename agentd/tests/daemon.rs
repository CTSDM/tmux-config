//! The daemon against a real, isolated tmux server (`tmux -L <unique>`, never
//! the one in $TMUX). Requests go straight to its socket. `@agents_bin` is
//! not set, so no helper (sound, notification) ever runs.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};
use std::{env, fs};

use agentd::identity::{self, Paths};
use serde_json::{Value, json};

static SERVERS: AtomicUsize = AtomicUsize::new(0);

struct Server {
    name: String,
    socket: String,
    pid: u32,
    /// Short: the daemon's socket path must fit in sun_path.
    runtime: PathBuf,
    paths: Paths,
    daemon: Option<Child>,
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

impl Server {
    fn start() -> Option<Server> {
        if !tmux_available() {
            eprintln!("tmux not found: skipped");
            return None;
        }
        let n = SERVERS.fetch_add(1, Ordering::SeqCst);
        let name = format!("agentd-it-{}-{n}", std::process::id());
        let runtime = PathBuf::from(format!("/tmp/agentd-it-{}-{n}", std::process::id()));
        fs::create_dir_all(&runtime).unwrap();
        let mut server = Server {
            name,
            socket: String::new(),
            pid: 0,
            paths: Paths::new(&runtime, "unset"),
            runtime,
            daemon: None,
        };
        server.tmux(&[
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
        server.socket = server.tmux(&["display", "-p", "#{socket_path}"]);
        server.pid = server.tmux(&["display", "-p", "#{pid}"]).parse().unwrap();
        let dir = identity::runtime_dir(Some(server.runtime.as_os_str()));
        server.paths = Paths::new(&dir, &identity::server_id(Path::new(&server.socket)));
        Some(server)
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

    fn start_daemon(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .arg("daemon")
            .env("TMUX", format!("{},{},0", self.socket, self.pid))
            .env_remove("TMUX_PANE")
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_STATE_HOME", self.runtime.join("state"))
            .env("AG_FOCUS_CLIENT", "none")
            .env("AG_SINK", self.runtime.join("sink.jsonl"))
            .env("AG_SOUND_PLAYER", "/bin/true")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
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

    /// SIGTERM, as a user stopping it; waits for it to exit.
    fn stop_daemon(&mut self) {
        let mut child = self.daemon.take().unwrap();
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        child.wait().unwrap();
    }

    fn call(&self, request: Value) -> Value {
        let mut stream = UnixStream::connect(&self.paths.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// A Claude hook event from `pane`, whose own process is the agent (I4).
    fn hook(&self, pane: &str, event: Value) {
        let pane_pid: u32 = self
            .tmux(&["display", "-p", "-t", pane, "#{pane_pid}"])
            .parse()
            .unwrap_or(1);
        let reply = self.call(json!({
            "v": 1, "kind": "claude", "pane": pane, "event": event,
            "chain": [[pane_pid, "claude", 0]], "env": {}, "t": 0,
        }));
        assert_eq!(reply["ok"], true, "{reply}");
    }

    fn status(&self) -> Value {
        self.call(json!({"v": 1, "ctl": "status"}))["data"].clone()
    }

    fn panes(&self) -> Vec<String> {
        let status = self.status();
        status["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect()
    }

    fn new_pane(&self) -> String {
        self.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "main:",
            "sleep 600",
        ])
    }

    fn start_time(&self) -> u64 {
        agentd::procfs::stat(self.pid).unwrap().starttime
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
        if let Some(mut child) = self.daemon.take() {
            // It follows the server (pidfd); don't leave it behind anyway.
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(format!(
            "/tmp/tmux-{}/{}",
            rustix::process::getuid().as_raw(),
            self.name
        ));
        let _ = fs::remove_dir_all(&self.runtime);
    }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(Instant::now() < deadline, "{what}: not within 5 s");
        sleep(Duration::from_millis(20));
    }
}

fn ev(name: &str) -> Value {
    json!({"hook_event_name": name, "session_id": "s1"})
}

#[test]
fn queues_go_after_session_end() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let pane = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    server.hook(&pane, ev("SessionStart"));
    assert_eq!(server.panes(), vec![pane.clone()]);
    server.hook(&pane, ev("SessionEnd"));
    wait_for("queues gone after SessionEnd", || server.panes().is_empty());
    // The pane lives on: a new session starts its queues again.
    server.hook(&pane, ev("SessionStart"));
    assert_eq!(server.panes(), vec![pane]);
}

#[test]
fn queues_go_when_the_pane_is_gone() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let pane = server.new_pane();
    server.hook(&pane, ev("UserPromptSubmit"));
    assert!(server.panes().contains(&pane));
    server.tmux(&["kill-pane", "-t", &pane]);
    // The next event of that pane finds it gone.
    server.hook(&pane, ev("Stop"));
    wait_for("queues gone with the pane", || {
        !server.panes().contains(&pane)
    });
}

#[test]
fn dead_panes_and_their_reminders_are_swept_at_a_stop() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.tmux(&["set", "-g", "@agent_remind_after", "600"]);
    server.start_daemon();
    let main = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    let dead = server.new_pane();
    server.hook(&main, ev("UserPromptSubmit"));
    server.hook(&dead, json!({"hook_event_name": "PermissionRequest", "session_id": "s2", "tool_name": "Bash", "tool_use_id": "t1"}));
    assert!(server.status()["reminders"].get(&dead).is_some());
    server.tmux(&["kill-pane", "-t", &dead]);
    // No event from the dead pane ever comes; another pane's Stop reads
    // every pane and drops what is gone.
    server.hook(&main, ev("Stop"));
    wait_for("dead pane swept", || !server.panes().contains(&dead));
    assert!(server.status()["reminders"].get(&dead).is_none());
    assert!(server.panes().contains(&main));
}

fn write_state(path: &Path, pid: u32, start: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let saved = json!({
        "v": 1,
        "server": {"pid": pid, "start": start},
        "core": {"subagents": {}, "rounds": {"work": ["%9"]}},
        "reminders": {},
    });
    fs::write(path, saved.to_string()).unwrap();
}

#[test]
fn state_of_another_server_instance_is_ignored() {
    let Some(mut server) = Server::start() else {
        return;
    };
    // Same socket path, another server instance (a restart): not ours.
    write_state(&server.paths.state, server.pid, server.start_time() + 1);
    server.start_daemon();
    assert_eq!(server.status()["state"]["rounds"], json!({}));
    server.stop_daemon();
    // Ours: read on start.
    write_state(&server.paths.state, server.pid, server.start_time());
    server.start_daemon();
    assert_eq!(server.status()["state"]["rounds"], json!({"work": ["%9"]}));
}

#[test]
fn state_file_kept_when_stopped_and_removed_when_the_server_dies() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let pane = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    server.hook(&pane, ev("UserPromptSubmit")); // joins a round: state to save
    wait_for("state file written", || server.paths.state.exists());
    server.stop_daemon();
    let saved: Value = serde_json::from_slice(&fs::read(&server.paths.state).unwrap()).unwrap();
    assert_eq!(
        saved["server"],
        json!({"pid": server.pid, "start": server.start_time()})
    );
    assert_eq!(saved["core"]["rounds"][""], json!([pane]));

    server.start_daemon();
    server.tmux(&["kill-server"]);
    let mut child = server.daemon.take().unwrap();
    wait_for("daemon exits with the server", || {
        child.try_wait().ok().flatten().is_some()
    });
    assert!(!server.paths.state.exists(), "state file left behind");
    assert!(!server.paths.socket.exists(), "socket left behind");
}

impl Server {
    fn ctl(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_agentd"))
            .arg("ctl")
            .args(args)
            .env("TMUX", format!("{},{},0", self.socket, self.pid))
            .env_remove("TMUX_PANE")
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("AG_FOCUS_CLIENT", "none")
            .env("AG_SOUND_PLAYER", "/bin/true")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .output()
            .unwrap()
    }

    fn pane_options(&self, pane: &str) -> Vec<String> {
        self.tmux(&["show", "-p", "-t", pane])
            .lines()
            .filter(|l| l.starts_with("@agent"))
            .map(String::from)
            .collect()
    }

    /// A pane whose process is named `claude`: a copy of dash (sleep may be
    /// a multicall binary that goes by its name), kept alive by `; :`.
    fn claude_pane(&self) -> String {
        let fake = self.runtime.join("claude");
        if !fake.exists() {
            fs::copy(fs::canonicalize("/bin/sh").unwrap(), &fake).unwrap();
        }
        let command = format!("{} -c 'sleep 600; :'", fake.display());
        let pane = self.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "main:",
            &command,
        ]);
        let pid: u32 = self
            .tmux(&["display", "-p", "-t", &pane, "#{pane_pid}"])
            .parse()
            .unwrap();
        wait_for("the fake claude", || {
            agentd::procfs::agent_of(pid).is_some()
        });
        pane
    }
}

#[test]
fn e2_reconcile_clears_a_pane_whose_agent_is_gone() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let pane = server.new_pane(); // runs sleep: no agent there
    for (name, value) in [
        ("@agent", "claude"),
        ("@agent_state", "working"),
        ("@agent_bg_watch", "1"),
    ] {
        server.tmux(&["set", "-p", "-t", &pane, name, value]);
    }
    let out = server.ctl(&["reconcile", &pane]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(server.pane_options(&pane), Vec::<String>::new());
}

#[test]
fn e2_reconcile_idles_a_busy_pane_whose_turn_is_over() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let pane = server.claude_pane();
    let transcript = server.runtime.join("t.jsonl");
    fs::write(&transcript, "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n").unwrap();
    for (name, value) in [
        ("@agent", "claude"),
        ("@agent_state", "needs"),
        ("@agent_needs", "permission"),
        ("@agent_tool", "Bash: ls"),
        ("@agent_transcript", transcript.to_str().unwrap()),
    ] {
        server.tmux(&["set", "-p", "-t", &pane, name, value]);
    }
    // Without panes: every agent pane.
    let out = server.ctl(&["reconcile"]);
    assert!(out.status.success(), "{out:?}");
    let options = server.pane_options(&pane);
    assert!(
        options.contains(&"@agent_state idle".to_string()),
        "{options:?}"
    );
    assert!(
        options
            .iter()
            .all(|o| !o.starts_with("@agent_needs") && !o.starts_with("@agent_tool")),
        "{options:?}"
    );
    // A turn that isn't over stays as it is.
    fs::write(&transcript, "{\"type\":\"assistant\"}\n").unwrap();
    server.tmux(&["set", "-p", "-t", &pane, "@agent_state", "working"]);
    server.ctl(&["reconcile", &pane]);
    assert!(
        server
            .pane_options(&pane)
            .contains(&"@agent_state working".to_string())
    );
}

#[test]
fn t2_6_bash_reconcile_hands_over_to_agentd() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let pane = server.new_pane();
    server.tmux(&["set", "-p", "-t", &pane, "@agent", "claude"]);
    server.tmux(&["set", "-g", "@agentd", env!("CARGO_BIN_EXE_agentd")]);
    let bash = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agents/bin/agent-reconcile");
    let out = Command::new(bash)
        .arg(&pane)
        .env("TMUX", format!("{},{},0", server.socket, server.pid))
        .env_remove("TMUX_PANE")
        .env("XDG_RUNTIME_DIR", &server.runtime)
        .env("XDG_STATE_HOME", server.runtime.join("state"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    // agentd cleared it (bash would too, but leaves @agent_bg_watch-like
    // internals; here the pane had only @agent).
    assert_eq!(server.pane_options(&pane), Vec::<String>::new());
    let status = server.status();
    assert!(status["panes"].as_array().unwrap().is_empty() || status["panes"] == json!([pane]));
}
