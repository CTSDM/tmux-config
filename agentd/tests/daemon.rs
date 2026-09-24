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

#[test]
fn b1_background_shells_are_counted_on_the_tick() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    // A fake claude that left one shell in the background, as Claude's Bash
    // tool starts them (its command line holds the snapshot path).
    let fake = server.runtime.join("claude");
    fs::copy(fs::canonicalize("/bin/sh").unwrap(), &fake).unwrap();
    let script = "sh -c 'sleep 600; :' shell-snapshots/snapshot-x & sleep 600; :";
    let command = format!("{} -c \"{script}\"", fake.display());
    let pane = server.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "main:",
        &command,
    ]);
    let pane_pid: u32 = server
        .tmux(&["display", "-p", "-t", &pane, "#{pane_pid}"])
        .parse()
        .unwrap();
    let mut agent = 0;
    wait_for("the fake claude", || {
        agent = agentd::procfs::agent_of(pane_pid).unwrap_or(0);
        agent != 0
    });
    let shell = || {
        agentd::procfs::children(agent).into_iter().find(|p| {
            agentd::procfs::entries(*p, "cmdline")
                .is_some_and(|a| a.join(" ").contains("shell-snapshots/snapshot-"))
        })
    };
    wait_for("the background shell", || shell().is_some());
    let chain = if agent == pane_pid {
        json!([[agent, "claude", 0]])
    } else {
        json!([[agent, "claude", 0], [pane_pid, "sh", 0]])
    };
    let reply = server.call(json!({
        "v": 1, "kind": "claude", "pane": pane, "event": {"hook_event_name": "Stop", "session_id": "s1"},
        "chain": chain, "env": {}, "t": 0,
    }));
    assert_eq!(reply["ok"], true);
    // Counted by the Stop itself, then watched.
    let bg = || server.tmux(&["show", "-pqv", "-t", &pane, "@agent_bg"]);
    assert_eq!(bg(), "1");
    assert_eq!(server.status()["background"][&pane], json!(1));
    // No bash watcher, no @agent_bg_watch (C3).
    assert!(
        server
            .tmux(&["show", "-p", "-t", &pane])
            .lines()
            .all(|l| !l.starts_with("@agent_bg_watch"))
    );
    // The shell ends: a tick later @agent_bg goes, and so does the watch.
    Command::new("kill")
        .arg(shell().unwrap().to_string())
        .status()
        .unwrap();
    wait_for("@agent_bg unset", || bg().is_empty());
    wait_for("no longer watched", || {
        server.status()["background"] == json!({})
    });
}

#[test]
fn t4_1_control_mode_session_of_its_own() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    assert_eq!(server.status()["transport"], "control");
    let sessions = |server: &Server| server.tmux(&["list-sessions", "-F", "#{session_name}"]);
    assert!(
        sessions(&server).lines().any(|s| s == "_peek-agentd"),
        "{}",
        sessions(&server)
    );
    assert_eq!(
        server.tmux(&["show", "-t", "=_peek-agentd:", "-v", "destroy-unattached"]),
        "on"
    );
    let clients = server.tmux(&[
        "list-clients",
        "-F",
        "#{client_control_mode} #{client_session} #{client_flags}",
    ]);
    assert!(clients.starts_with("1 _peek-agentd "), "{clients}");
    assert!(
        clients.contains("ignore-size")
            && clients.contains("no-output")
            && clients.contains("UTF-8"),
        "{clients}"
    );
    // Killed from outside: it attaches again, and answers meanwhile.
    server.tmux(&["kill-session", "-t", "=_peek-agentd"]);
    let pane = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    server.hook(&pane, ev("SessionStart"));
    wait_for("attached again", || {
        server.hook(&pane, ev("UserPromptSubmit"));
        sessions(&server).lines().any(|s| s == "_peek-agentd")
    });
    assert_eq!(
        server.tmux(&["show", "-p", "-t", &pane, "-qv", "@agent_state"]),
        "working"
    );
    // Gone with the daemon.
    server.stop_daemon();
    wait_for("its session gone", || {
        !sessions(&server).lines().any(|s| s == "_peek-agentd")
    });
    assert!(sessions(&server).lines().any(|s| s == "main"));
}

#[test]
fn t4_3_blink_takes_turns_with_agent_blink() {
    let Some(mut server) = Server::start() else {
        return;
    };
    // agent-blink's lock, held as a running agent-blink holds it.
    let dir = identity::runtime_dir(Some(server.runtime.as_os_str()));
    fs::create_dir_all(&dir).unwrap();
    let socket_name = Path::new(&server.socket).file_name().unwrap();
    let lock = fs::File::create(dir.join(format!("blink-{}.lock", socket_name.display()))).unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    // Left lit by an animator that is gone.
    server.tmux(&["new-session", "-d", "-s", "old", "sleep 600"]);
    server.tmux(&["set", "-t", "=old:", "@blink-s", "1"]);
    let pane = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    server.tmux(&["set", "-p", "-t", &pane, "@agent_state", "needs"]);
    let blink = |server: &Server, session: &str| -> Vec<String> {
        server
            .tmux(&["show", "-t", &format!("={session}:")])
            .lines()
            .filter(|l| l.starts_with("@blink"))
            .map(String::from)
            .collect()
    };

    server.start_daemon();
    sleep(Duration::from_millis(1500));
    assert_eq!(
        blink(&server, "main"),
        [] as [String; 0],
        "agent-blink draws"
    );
    drop(lock);
    wait_for("the daemon takes over", || {
        blink(&server, "main").contains(&"@blink-s-kind needs".to_string())
    });
    assert_eq!(blink(&server, "old"), [] as [String; 0]);
    assert!(server.status()["blinking"].to_string().contains("main"));
    // Stopped: nothing left lit.
    server.stop_daemon();
    assert_eq!(blink(&server, "main"), [] as [String; 0]);
}

/// Terminals for the server's clients: an outer isolated tmux whose panes run
/// `tmux attach` against it, as a person's terminals would.
struct Terminals {
    name: String,
    inner: String,
    count: usize,
}

impl Terminals {
    fn new(server: &Server) -> Terminals {
        Terminals {
            name: format!("{}-terminals", server.name),
            inner: server.name.clone(),
            count: 0,
        }
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .arg("-L")
            .arg(&self.name)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            // Its panes run the commands with sh, not the user's shell.
            .env("SHELL", "/bin/sh")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    /// A terminal running `tmux <args>` against the server.
    fn open(&mut self, args: &str) {
        let command = format!(
            "env -u TMUX -u TMUX_PANE TERM=xterm-256color tmux -L {} {args}",
            self.inner
        );
        let window = format!("t{}", self.count);
        if self.count == 0 {
            self.tmux(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "t",
                "-n",
                &window,
                "-x",
                "100",
                "-y",
                "30",
                &command,
            ]);
        } else {
            self.tmux(&["new-window", "-d", "-t", "t:", "-n", &window, &command]);
        }
        self.count += 1;
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
        let _ = fs::remove_file(format!(
            "/tmp/tmux-{}/{}",
            rustix::process::getuid().as_raw(),
            self.name
        ));
    }
}

impl Server {
    /// agents.conf's Z1 section, sourced alone (the rest runs bash helpers).
    fn source_z1(&self) {
        let conf = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../agents/agents.conf"
        ))
        .unwrap();
        let start = conf
            .find("# --- Out of sight")
            .expect("Z1 section in agents.conf");
        let end = conf[start + 1..]
            .find("\n# ---")
            .map_or(conf.len(), |e| start + 1 + e);
        let path = self.runtime.join("z1.conf");
        fs::write(&path, &conf[start..end]).unwrap();
        self.tmux(&["source-file", path.to_str().unwrap()]);
    }

    /// (name, session) of the terminal clients.
    fn terminal_clients(&self) -> Vec<(String, String)> {
        self.tmux(&[
            "list-clients",
            "-F",
            "#{client_control_mode} #{client_name} #{client_session}",
        ])
        .lines()
        .filter_map(|l| l.strip_prefix("0 "))
        .filter_map(|l| l.split_once(' '))
        .map(|(c, s)| (c.to_string(), s.to_string()))
        .collect()
    }

    fn session_of(&self, client: &str) -> String {
        self.tmux(&["display", "-p", "-c", client, "#{client_session}"])
    }
}

#[test]
fn t4_5_plain_attach_lands_in_a_session_of_the_user() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.source_z1();
    server.tmux(&["new-session", "-d", "-s", "work", "sleep 600"]);
    let mut terminals = Terminals::new(&server);
    terminals.open("attach -t main");
    terminals.open("attach -t work");
    wait_for("two terminals", || server.terminal_clients().len() == 2);
    server.start_daemon();
    // Every session of the user has a client: tmux would pick the most recent, ours.
    wait_for("our session", || {
        server.tmux(&["display", "-p", "#{s/ .*//:#{S/t:#{session_name} }}"]) == "_peek-agentd"
    });

    let before: Vec<(String, String)> = server.terminal_clients();
    terminals.open("attach");
    wait_for("a third terminal", || server.terminal_clients().len() == 3);
    let (plain, _) = server
        .terminal_clients()
        .into_iter()
        .find(|c| !before.contains(c))
        .unwrap();
    let user = |s: &str| s == "main" || s == "work";
    wait_for("the plain attach in a session of the user", || {
        user(&server.session_of(&plain))
    });
    let landed = server.session_of(&plain);

    // Back, and into ours by name: it goes on to the user's most recent session.
    server.tmux(&["switch-client", "-c", &plain, "-l"]);
    sleep(Duration::from_millis(200));
    assert!(
        user(&server.session_of(&plain)),
        "{}",
        server.session_of(&plain)
    );
    server.tmux(&["switch-client", "-c", &plain, "-t", "=_peek-agentd:"]);
    sleep(Duration::from_millis(200));
    assert_eq!(server.session_of(&plain), landed);
    assert_eq!(
        server.tmux(&[
            "display",
            "-p",
            "-t",
            "=_peek-agentd:",
            "#{session_attached}"
        ]),
        "1",
        "only our client"
    );

    // agent-peek's sessions are left alone.
    terminals.open("new-session -t =main -s _peek-9");
    wait_for("the peek client", || {
        server
            .terminal_clients()
            .iter()
            .any(|(_, s)| s == "_peek-9")
    });
    sleep(Duration::from_millis(200));
    assert!(
        server
            .terminal_clients()
            .iter()
            .any(|(_, s)| s == "_peek-9")
    );
}

#[test]
fn t4_5_choosers_hide_our_session_and_client() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.source_z1();
    server.tmux(&["new-window", "-d", "-t", "main:", "sleep 600"]);
    server.start_daemon();
    let mut terminals = Terminals::new(&server);
    terminals.open("attach -t main");
    wait_for("a terminal", || server.terminal_clients().len() == 1);
    // The keys pressed in the terminal, what it shows.
    for key in ["s", "w", "D"] {
        terminals.tmux(&["send-keys", "-t", "t:t0", "C-b", key]);
        sleep(Duration::from_millis(400));
        let screen = terminals.tmux(&["capture-pane", "-p", "-t", "t:t0"]);
        assert!(
            screen.contains("(filter: active)"),
            "prefix {key}: {screen}"
        );
        assert!(!screen.contains("_peek-agentd"), "prefix {key}: {screen}");
        assert!(!screen.contains("client-"), "prefix {key}: {screen}");
        terminals.tmux(&["send-keys", "-t", "t:t0", "q"]);
        sleep(Duration::from_millis(200));
    }
}

#[test]
fn t4_5_the_last_session_closing_takes_the_server() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    server.tmux(&["kill-session", "-t", "=main:"]);
    wait_for("the server gone", || {
        !Command::new("tmux")
            .args(["-L", &server.name, "has-session"])
            .env_remove("TMUX")
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
            && server.tmux(&["list-sessions"]).is_empty()
            && procfs_gone(server.pid)
    });
    let daemon = server.daemon.as_mut().unwrap();
    wait_for("the daemon gone", || daemon.try_wait().unwrap().is_some());
}

fn procfs_gone(pid: u32) -> bool {
    agentd::procfs::stat(pid).is_none_or(|s| s.state == 'Z')
}

#[test]
fn t4_5_back_when_the_user_has_a_session_again() {
    let Some(mut server) = Server::start() else {
        return;
    };
    // The server stays without sessions.
    server.tmux(&["set", "-g", "exit-empty", "off"]);
    server.start_daemon();
    server.tmux(&["kill-session", "-t", "=main:"]);
    wait_for("our session closed too", || {
        server
            .tmux(&["list-sessions", "-F", "#{session_name}"])
            .is_empty()
    });
    assert_eq!(server.status()["transport"], "spawn");
    sleep(Duration::from_millis(1200));
    assert!(
        server
            .tmux(&["list-sessions", "-F", "#{session_name}"])
            .is_empty()
    );

    server.tmux(&["new-session", "-d", "-s", "back", "sleep 600"]);
    let pane = server.tmux(&["display", "-p", "-t", "back", "#{pane_id}"]);
    server.hook(&pane, ev("SessionStart"));
    wait_for("attached again", || {
        server.hook(&pane, ev("UserPromptSubmit"));
        server.status()["transport"] == "control"
    });
    assert!(
        server
            .tmux(&["list-sessions", "-F", "#{session_name}"])
            .lines()
            .any(|s| s == "_peek-agentd")
    );
}
