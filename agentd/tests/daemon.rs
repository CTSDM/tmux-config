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
        Server::start_with(&[
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
        ])
    }

    /// A server that stays without sessions (exit-empty off), as while tmux
    /// loads its configuration before the first one.
    fn start_empty() -> Option<Server> {
        let conf = env::temp_dir().join(format!("agentd-it-{}-empty.conf", std::process::id()));
        fs::write(&conf, "set -g exit-empty off\n").unwrap();
        let server = Server::start_with(&["-f", conf.to_str().unwrap(), "start-server"]);
        let _ = fs::remove_file(&conf);
        server
    }

    fn start_with(first: &[&str]) -> Option<Server> {
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
        server.tmux(first);
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

#[test]
fn a_killed_pane_leaves_nothing_behind() {
    let Some(mut server) = Server::start() else {
        return;
    };
    let main = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    let dead = server.new_pane();
    server.tmux(&["kill-pane", "-t", &dead]);
    // What a killed pane leaves when no SessionEnd ever comes for it.
    fs::create_dir_all(server.paths.state.parent().unwrap()).unwrap();
    let saved = json!({
        "v": 1,
        "server": {"pid": server.pid, "start": server.start_time()},
        "core": {
            "subagents": {"gone": {"a1": "Explore"}},
            "rounds": {"work": [dead]},
            "codex": {(dead.clone()): {"session": "c1"}},
        },
        "reminders": {},
        "notifications": {(dead.clone()): 7},
    });
    fs::write(&server.paths.state, saved.to_string()).unwrap();
    server.start_daemon();
    // One sweep that misses them is not enough: its list may predate a new
    // pane or session. The next one forgets them.
    server.hook(&main, ev("Stop"));
    let state = &server.status()["state"];
    assert_eq!(state["codex"].as_object().unwrap().len(), 1);
    assert_eq!(state["subagents"], json!({"gone": {"a1": "Explore"}}));
    server.hook(&main, ev("Stop"));
    let state = &server.status()["state"];
    assert_eq!(state["codex"], json!({}));
    assert_eq!(state["rounds"], json!({}));
    assert_eq!(state["subagents"], json!({}));
    let sink = server.runtime.join("sink.jsonl");
    wait_for("the dead pane's notification closed", || {
        fs::read_to_string(&sink).is_ok_and(|s| {
            s.lines().any(|l| {
                let v: Value = serde_json::from_str(l).unwrap_or_default();
                v["effect"] == "notify-close" && v["pane"] == dead.as_str()
            })
        })
    });
}

#[test]
fn a_session_on_a_live_pane_keeps_its_subagents() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let main = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    let other = server.new_pane();
    server.hook(
        &other,
        json!({"hook_event_name": "SessionStart", "session_id": "s2"}),
    );
    server.hook(&other, json!({"hook_event_name": "SubagentStart", "session_id": "s2", "agent_id": "a1", "agent_type": "Explore"}));
    server.hook(&main, ev("Stop"));
    server.hook(&main, ev("Stop"));
    assert_eq!(
        server.status()["state"]["subagents"],
        json!({"s2": {"a1": "Explore"}})
    );
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
fn h5b_a_shell_after_the_dialog_is_its_answer() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    // A fake claude with a shell of its Bash tool already running (in the
    // background, say), and one more once `go` exists: the answer.
    let fake = server.runtime.join("claude");
    fs::copy(fs::canonicalize("/bin/sh").unwrap(), &fake).unwrap();
    let go = server.runtime.join("go");
    let script = format!(
        "sh -c 'sleep 600; :' shell-snapshots/snapshot-a & \
         while [ ! -e {go} ]; do sleep 0.1; done; \
         sh -c 'sleep 600; :' shell-snapshots/snapshot-b & sleep 600; :",
        go = go.display()
    );
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
    let shells = || agentd::procfs::count_children_matching(agent, "shell-snapshots/snapshot-");
    wait_for("the first shell", || shells() == 1);
    let chain = if agent == pane_pid {
        json!([[agent, "claude", 0]])
    } else {
        json!([[agent, "claude", 0], [pane_pid, "sh", 0]])
    };
    let hook = |event: Value| {
        let reply = server.call(json!({
            "v": 1, "kind": "claude", "pane": pane, "event": event, "chain": chain, "env": {}, "t": 0,
        }));
        assert_eq!(reply["ok"], true);
    };
    let state = || server.tmux(&["show", "-pqv", "-t", &pane, "@agent_state"]);
    hook(ev("UserPromptSubmit"));
    hook(
        json!({"hook_event_name": "PermissionRequest", "session_id": "s1",
        "tool_name": "Bash", "tool_use_id": "t1"}),
    );
    assert_eq!(state(), "needs");
    assert_eq!(server.status()["answering"], json!([pane]));
    // The shell that ran before, and the loop's own children, are no answer.
    sleep(Duration::from_millis(2500));
    assert_eq!(state(), "needs");
    fs::write(&go, "").unwrap();
    wait_for("working", || state() == "working");
    assert_eq!(shells(), 2);
    assert_eq!(
        server.tmux(&["show", "-pqv", "-t", &pane, "@agent_needs"]),
        ""
    );
    assert_eq!(server.status()["answering"], json!([]));
    let log = fs::read_to_string(server.runtime.join("state/tmux-agents/events.log")).unwrap();
    assert!(
        log.lines()
            .any(|l| l.ends_with(&format!("{pane} claude answered needs:permission->working"))),
        "{log}"
    );
    // A question is answered in the dialog itself: not watched. Leaving the
    // wait (for a question, for the end of the turn) ends the watch.
    hook(
        json!({"hook_event_name": "PermissionRequest", "session_id": "s1",
        "tool_name": "AskUserQuestion", "tool_use_id": "q1"}),
    );
    assert_eq!(server.status()["answering"], json!([]));
    hook(
        json!({"hook_event_name": "Notification", "session_id": "s1",
        "notification_type": "permission_prompt"}),
    );
    assert_eq!(server.status()["answering"], json!([]), "already waiting");
    hook(json!({"hook_event_name": "PostToolUse", "session_id": "s1",
        "tool_name": "AskUserQuestion", "tool_use_id": "q1"}));
    hook(
        json!({"hook_event_name": "Notification", "session_id": "s1",
        "notification_type": "permission_prompt"}),
    );
    assert_eq!(server.status()["answering"], json!([pane]));
    hook(ev("Stop"));
    assert_eq!(server.status()["answering"], json!([]));
}

#[test]
fn event_log_has_structure_and_no_text() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let pane = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    let canary = "canary-private-words";
    let with = |name: &str, fields: Value| {
        let mut e = ev(name);
        e.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        server.hook(&pane, e);
    };
    with(
        "SessionStart",
        json!({"source": "startup", "transcript_path": format!("/home/{canary}.jsonl")}),
    );
    with("UserPromptSubmit", json!({"prompt": canary}));
    with(
        "PreToolUse",
        json!({"tool_name": "Bash", "tool_use_id": "t1", "detail": canary}),
    );
    with(
        "PermissionRequest",
        json!({"tool_name": "Bash", "permission_mode": "bypassPermissions", "detail": canary}),
    );
    with(
        "Notification",
        json!({"notification_type": "permission_prompt", "message": canary}),
    );
    with(
        "SubagentStart",
        json!({"agent_id": "a1", "agent_type": "Explore"}),
    );
    with(
        "Stop",
        json!({"last_assistant_message": canary, "error": canary}),
    );
    // Through the real hook client, from a process that is not the pane's
    // agent (I4): logged as ignored, with the payload's texts left behind.
    let payload = json!({
        "hook_event_name": "PreToolUse", "session_id": canary, "tool_name": "Write",
        "tool_input": {"file_path": format!("/{canary}"), "content": canary}, "prompt": canary,
    });
    let mut hook = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["hook", "claude"])
        .env("TMUX", format!("{},{},0", server.socket, server.pid))
        .env("TMUX_PANE", &pane)
        .env("XDG_RUNTIME_DIR", &server.runtime)
        .env("XDG_STATE_HOME", server.runtime.join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    assert!(hook.wait().unwrap().success());

    let log = fs::read_to_string(server.runtime.join("state/tmux-agents/events.log")).unwrap();
    assert!(!log.contains(canary), "{log}");
    assert!(!log.contains("/home"), "{log}");
    let lines: Vec<&str> = log.lines().collect();
    let has = |what: &str| {
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!(" {pane} ")) && l.ends_with(what)),
            "no line ending in {what:?}:\n{log}"
        )
    };
    assert!(lines[0].ends_with(&format!(
        "agentd start pid={}",
        server.daemon.as_ref().unwrap().id()
    )));
    has("claude SessionStart source=startup none->ready");
    has("claude UserPromptSubmit ready->working");
    has("claude PreToolUse tool=Bash tool_use_id=yes working->working");
    has(
        "claude PermissionRequest mode=bypassPermissions tool=Bash tool_use_id=no working->working",
    );
    has("claude Notification type=permission_prompt working->needs:permission");
    has("claude SubagentStart subagent=yes needs:permission->needs:permission");
    has("claude Stop needs:permission->done");
    has("claude PreToolUse tool=Write tool_use_id=no ignored:not-its-agent");
    // Time, then the server's socket name.
    let first = lines[1].split(' ').collect::<Vec<_>>();
    assert_eq!(first[0].len(), "2026-09-25".len(), "{}", lines[1]);
    assert_eq!(first[1].len(), "11:00:47.123".len(), "{}", lines[1]);
    assert_eq!(first[2], server.name);
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
        self.source_section("# --- Out of sight");
    }

    /// One section of agents.conf (from its `# ---` header to the next).
    fn source_section(&self, header: &str) {
        let conf = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../agents/agents.conf"
        ))
        .unwrap();
        let start = conf.find(header).expect("the section in agents.conf");
        let end = conf[start + 1..]
            .find("\n# ---")
            .map_or(conf.len(), |e| start + 1 + e);
        let path = self.runtime.join("section.conf");
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

/// Bash's hook in this checkout.
const BASH_HOOK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../agents/bin/agent-hook");

impl Server {
    /// What panes and hooks inherit: this test's runtime and state, the
    /// sink, no desktop.
    fn safe_env(&self) {
        let state = self.runtime.join("state");
        fs::create_dir_all(state.join("tmux-agents")).unwrap();
        for (name, value) in [
            ("XDG_RUNTIME_DIR", self.runtime.clone()),
            ("XDG_STATE_HOME", state),
            ("HOME", self.runtime.join("home")),
            ("AG_SINK", self.runtime.join("sink.jsonl")),
            ("AG_FOCUS_CLIENT", "none".into()),
            ("AG_SOUND_PLAYER", "/bin/true".into()),
        ] {
            self.tmux(&["set-environment", "-g", name, value.to_str().unwrap()]);
        }
        for name in [
            "DBUS_SESSION_BUS_ADDRESS",
            "WAYLAND_DISPLAY",
            "DISPLAY",
            "HYPRLAND_INSTANCE_SIGNATURE",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
        ] {
            self.tmux(&["set-environment", "-gu", name]);
        }
    }

    fn off_file(&self) -> PathBuf {
        self.runtime.join("state/tmux-agents/agentd.off")
    }

    /// A pane whose agent (a fake `claude`) runs `<hook> claude` for each
    /// line written to the fifo it returns: the hook is its child, as with
    /// Claude Code.
    fn agent_running(&self, hook: &str) -> (String, PathBuf) {
        let fake = self.runtime.join("claude");
        if !fake.exists() {
            fs::copy(fs::canonicalize("/bin/sh").unwrap(), &fake).unwrap();
        }
        let fifo = self
            .runtime
            .join(format!("events-{}", SERVERS.fetch_add(1, Ordering::SeqCst)));
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let script = format!(
            "while read -r event < {fifo}; do printf '%s\\n' \"$event\" | {hook} claude; done; sleep 600",
            fifo = fifo.display()
        );
        // Separate words: tmux runs it without a shell in between.
        let pane = self.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "main:",
            fake.to_str().unwrap(),
            "-c",
            &script,
        ]);
        (pane, fifo)
    }
}

fn send_event(fifo: &Path, event: Value) {
    let mut f = fs::OpenOptions::new().write(true).open(fifo).unwrap();
    writeln!(f, "{event}").unwrap();
}

#[test]
fn t5_1_agent_hook_hands_over_to_agentd() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.safe_env();
    server.tmux(&["set", "-g", "@agentd", env!("CARGO_BIN_EXE_agentd")]);
    server.start_daemon();
    // An agent started before the switch: its hook is still bash's.
    let (pane, fifo) = server.agent_running(BASH_HOOK);
    send_event(&fifo, ev("UserPromptSubmit"));
    wait_for("working", || {
        server.tmux(&["show", "-p", "-t", &pane, "-qv", "@agent_state"]) == "working"
    });
    assert!(
        server.panes().contains(&pane),
        "the daemon's: {}",
        server.status()
    );
}

#[test]
fn t5_1_old_bash_codex_observers_are_dropped() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.safe_env();
    server.tmux(&["set", "-g", "@agentd", env!("CARGO_BIN_EXE_agentd")]);
    server.start_daemon();
    let pane = server.new_pane();
    // What a bash `agent-codex watch` of before runs: detached, not a
    // descendant of the agent.
    let mut observer = Command::new("setsid")
        .args([BASH_HOOK, "codex", "--observe"])
        .env("TMUX", format!("{},{},0", server.socket, server.pid))
        .env("TMUX_PANE", &pane)
        .env("XDG_RUNTIME_DIR", &server.runtime)
        .env("XDG_STATE_HOME", server.runtime.join("state"))
        .env("AG_SINK", server.runtime.join("sink.jsonl"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let payload =
        json!({"hook_event_name": "UserPromptSubmit", "session_id": "c1", "turn_id": "t1"});
    writeln!(observer.stdin.take().unwrap(), "{payload}").unwrap();
    assert!(observer.wait().unwrap().success());
    // It reached agentd, whose process-tree check (I4) dropped it.
    wait_for("the daemon saw it", || server.panes().contains(&pane));
    assert_eq!(server.pane_options(&pane), [] as [String; 0]);
    // And bash kept no Codex bookkeeping for it.
    let bash_files: Vec<_> = fs::read_dir(server.runtime.join("tmux-agents"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("agentd-") && !n.starts_with("blink-"))
        .collect();
    assert_eq!(bash_files, [] as [String; 0]);
}

#[test]
fn t5_2_agentd_off_hands_hooks_to_bash_and_starts_nothing() {
    let Some(server) = Server::start() else {
        return;
    };
    server.safe_env();
    // @agentd still set: bash must not hand the event back.
    server.tmux(&["set", "-g", "@agentd", env!("CARGO_BIN_EXE_agentd")]);
    fs::write(server.off_file(), "").unwrap();
    let (pane, fifo) = server.agent_running(&format!("{} hook", env!("CARGO_BIN_EXE_agentd")));
    send_event(&fifo, ev("UserPromptSubmit"));
    wait_for("working, by bash", || {
        server.tmux(&["show", "-p", "-t", &pane, "-qv", "@agent_state"]) == "working"
    });
    assert!(!server.paths.socket.exists(), "no daemon started");

    let env = |c: &mut Command| {
        c.env("TMUX", format!("{},{},0", server.socket, server.pid))
            .env("XDG_RUNTIME_DIR", &server.runtime)
            .env("XDG_STATE_HOME", server.runtime.join("state"))
            .env_remove("TMUX_PANE")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    };
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_agentd"));
    daemon.arg("daemon");
    env(&mut daemon);
    let started = Instant::now();
    assert!(daemon.status().unwrap().success());
    assert!(started.elapsed() < Duration::from_secs(1));
    let mut ensure = Command::new(env!("CARGO_BIN_EXE_agentd"));
    ensure.arg("ensure");
    env(&mut ensure);
    assert!(ensure.status().unwrap().success());
    assert!(!server.paths.socket.exists(), "no daemon started");
}

#[test]
fn t5_3_ctl_stop_saves_and_exits() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let pane = server.tmux(&["display", "-p", "-t", "main", "#{pane_id}"]);
    server.hook(&pane, ev("SessionStart"));
    let out = server.ctl(&["stop"]);
    assert!(out.status.success(), "{out:?}");
    let daemon = server.daemon.as_mut().unwrap();
    wait_for("the daemon gone", || daemon.try_wait().unwrap().is_some());
    assert!(server.paths.state.exists(), "state saved");
    assert!(!server.paths.socket.exists());
    // Nothing runs: still fine.
    assert!(server.ctl(&["stop"]).status.success());
}

#[test]
fn t4_5_a_daemon_before_the_first_session_stays() {
    let Some(mut server) = Server::start_empty() else {
        return;
    };
    // Started while tmux loads its configuration: ours is the only session.
    server.start_daemon();
    sleep(Duration::from_millis(300));
    server.tmux(&["new-session", "-d", "-s", "main", "sleep 600"]);
    sleep(Duration::from_millis(1000));
    assert_eq!(server.status()["transport"], "control");
    let sessions = server.tmux(&["list-sessions", "-F", "#{session_name}"]);
    assert!(sessions.lines().any(|s| s == "_peek-agentd"), "{sessions}");
}

#[test]
fn f1_focus_reaches_agentd_without_a_process() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.safe_env();
    // @agentd names a wrapper that logs each run: the processes the hooks start.
    let runs = server.runtime.join("agentd-runs");
    let wrapper = server.runtime.join("agentd-wrapper");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho \"$*\" >> {}\nexec {} \"$@\"\n",
            runs.display(),
            env!("CARGO_BIN_EXE_agentd")
        ),
    )
    .unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(&wrapper)
        .status()
        .unwrap();
    server.tmux(&["set", "-g", "@agentd", wrapper.to_str().unwrap()]);
    server.tmux(&[
        "set",
        "-g",
        "@agents_bin",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../agents/bin"),
    ]);
    server.tmux(&["set", "-g", "focus-events", "on"]);
    server.start_daemon();
    // The daemon names its control client.
    let control = |server: &Server| {
        server
            .tmux(&[
                "list-clients",
                "-F",
                "#{client_control_mode} #{client_name}",
            ])
            .lines()
            .find_map(|l| l.strip_prefix("1 ").map(String::from))
            .unwrap_or_default()
    };
    wait_for("@agentd_client", || {
        let named = server.tmux(&["show", "-gqv", "@agentd_client"]);
        !named.is_empty() && named == control(&server)
    });
    server.source_section("# --- Seen / not seen");

    let pane = server.claude_pane();
    let state = |server: &Server| server.tmux(&["show", "-p", "-t", &pane, "-qv", "@agent_state"]);
    let finish = |server: &Server| {
        server.hook(&pane, ev("UserPromptSubmit"));
        server.hook(&pane, ev("Stop"));
        assert_eq!(state(server), "done");
    };
    server.hook(&pane, ev("SessionStart"));
    finish(&server);
    // A client that has just connected (tmux names it only once it identifies):
    // the hook must not read its name (tmux 3.6 crashes on it in a #{L:} loop).
    let _connecting = UnixStream::connect(&server.socket).unwrap();
    // A terminal on main, with focus; then the agent's window comes to the front.
    let mut terminals = Terminals::new(&server);
    terminals.open("attach -t main");
    wait_for("a terminal", || server.terminal_clients().len() == 1);
    terminals.tmux(&["send-keys", "-t", "t:t0", "-H", "1b", "5b", "49"]);
    sleep(Duration::from_millis(200));
    server.tmux(&["select-window", "-t", &pane]);
    wait_for("seen, through the message", || state(&server) == "idle");
    assert_eq!(
        server.tmux(&["display", "-p", "ok"]),
        "ok",
        "the server lives"
    );
    let started = || fs::read_to_string(&runs).unwrap_or_default();
    assert!(!started().contains("ctl seen"), "{}", started());

    // No live client of agentd's (the spawn transport): `ctl seen`, a process.
    server.tmux(&["set", "-g", "@agentd_client", "client-gone"]);
    finish(&server);
    server.tmux(&["select-window", "-t", "main:^"]);
    server.tmux(&["select-window", "-t", &pane]);
    wait_for("seen, through ctl seen", || state(&server) == "idle");
    assert!(
        started().contains(&format!("ctl seen {pane}")),
        "{}",
        started()
    );

    // Gone with the daemon.
    server.stop_daemon();
    assert_eq!(server.tmux(&["show", "-gqv", "@agentd_client"]), "");
}

#[test]
fn f2_a_switch_lays_out_only_for_another_width() {
    let Some(server) = Server::start() else {
        return;
    };
    server.safe_env();
    // agent-spaces, as a stand-in that logs what it is asked.
    let bin = server.runtime.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let runs = server.runtime.join("spaces-runs");
    fs::write(
        bin.join("agent-spaces"),
        format!("#!/bin/sh\necho \"$*\" >> {}\n", runs.display()),
    )
    .unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(bin.join("agent-spaces"))
        .status()
        .unwrap();
    server.tmux(&["set", "-g", "@agents_bin", bin.to_str().unwrap()]);
    server.tmux(&["new-session", "-d", "-s", "work", "sleep 600"]);
    server.source_section("# --- Spaces");
    let mut terminals = Terminals::new(&server);
    terminals.open("attach -t main");
    wait_for("a terminal", || server.terminal_clients().len() == 1);
    let (client, _) = server.terminal_clients().remove(0);
    let width = server.tmux(&["display", "-p", "-c", &client, "#{client_width}"]);
    let layouts = || {
        fs::read_to_string(&runs)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == "layout")
            .count()
    };
    wait_for("the attach's layout", || layouts() >= 1);
    sleep(Duration::from_millis(200));
    let before = layouts();
    // A client that has just connected: see f1_focus_reaches_agentd_without_a_process.
    let _connecting = UnixStream::connect(&server.socket).unwrap();

    // work was laid out for this width: no process.
    server.tmux(&["set", "-t", "=work:", "@layout-width", &width]);
    server.tmux(&["switch-client", "-c", &client, "-t", "=work:"]);
    sleep(Duration::from_millis(300));
    assert_eq!(layouts(), before);
    // main was laid out for another one: a layout.
    server.tmux(&["set", "-t", "=main:", "@layout-width", "1"]);
    server.tmux(&["switch-client", "-c", &client, "-t", "=main:"]);
    wait_for("a layout", || layouts() == before + 1);
    assert_eq!(
        server.tmux(&["display", "-p", "ok"]),
        "ok",
        "the server lives"
    );
    // A window opened and one closed change the tab rows: a layout each.
    let window = server.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{window_id}",
        "-t",
        "main:",
        "sleep 600",
    ]);
    wait_for("a layout for the new window", || layouts() == before + 2);
    server.tmux(&["kill-window", "-t", &window]);
    wait_for("a layout for the closed window", || layouts() == before + 3);
    // A rename is not one of them.
    server.tmux(&["rename-window", "-t", "main:^", "renamed"]);
    sleep(Duration::from_millis(300));
    assert_eq!(layouts(), before + 3);
}

#[test]
fn l1_the_top_rows_values_follow_the_panes() {
    let Some(mut server) = Server::start() else {
        return;
    };
    for s in ["alpha", "zulu"] {
        server.tmux(&["new-session", "-d", "-s", s, "sleep 600"]);
    }
    for (s, space) in [("main", "work"), ("alpha", "work"), ("zulu", "home")] {
        server.tmux(&["set", "-t", &format!("={s}:"), "@space_auto", space]);
    }
    server.start_daemon();
    let get = |target: &str, option: &str| server.tmux(&["show", "-qv", "-t", target, option]);
    let alpha = server.tmux(&["display", "-p", "-t", "alpha", "#{pane_id}"]);
    server.hook(&alpha, ev("UserPromptSubmit"));
    // Right after our write: the other session of the space counts it,
    // the session itself shows its glyph, another space nothing.
    wait_for("main counts alpha working", || {
        get("main:", "@s-other-working") == "1"
    });
    assert_eq!(get("alpha:", "@s-glyphs"), "#[fg=#{@ac-working}]●");
    assert_eq!(get("alpha:", "@s-other-working"), "");
    assert_eq!(get("zulu:", "@s-other-working"), "");

    // An agent without hooks (a program named claude), in a new window: our
    // control client hears of the window.
    let claude = server.runtime.join("claude");
    fs::copy("/bin/sh", &claude).unwrap();
    let untracked = server.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "alpha:",
        &format!("{} -c 'sleep 600; :'", claude.display()),
    ]);
    wait_for("main counts it untracked", || {
        get("main:", "@s-other-untracked") == "1"
    });
    let flag = server.tmux(&["show", "-pqv", "-t", &untracked, "@p-untracked"]);
    assert_eq!(flag, "1");
    assert_eq!(
        get("alpha:", "@s-glyphs"),
        "#[fg=#{@ac-working}]●#[fg=#{@ac-dim}]◇"
    );

    // A pane killed: agents.conf's hook tells our client.
    server.tmux(&["kill-pane", "-t", &alpha]);
    let client = server.tmux(&["show", "-gqv", "@agentd_client"]);
    server.tmux(&["display-message", "-c", &client, "agentd bar"]);
    wait_for("alpha's working agent gone from main's count", || {
        get("main:", "@s-other-working").is_empty()
    });
    assert_eq!(get("alpha:", "@s-glyphs"), "#[fg=#{@ac-dim}]◇");

    // alpha moves to zulu's space, as agent-spaces does it.
    server.tmux(&["set", "-t", "=alpha:", "@space", "home"]);
    let reply = server.call(json!({"v": 1, "ctl": "bar"}));
    assert_eq!(reply["ok"], true, "{reply}");
    wait_for("zulu counts alpha's agent", || {
        get("zulu:", "@s-other-untracked") == "1"
    });
    assert_eq!(get("main:", "@s-other-untracked"), "");
    // Ours has none of them.
    assert_eq!(get("_peek-agentd:", "@s-other-untracked"), "");
}

/// I6: a remote pane's event, as its `agentd remote` (the chain) sends it.
fn remote_hook(server: &Server, pane: &str, kind: &str, chain: Value, event: Value) -> Value {
    server.call(json!({
        "v": 1, "kind": kind, "pane": pane, "event": event, "chain": chain,
        "env": {}, "t": 0, "remote": {"host": "box", "name": "api", "agent": 4242},
    }))
}

#[test]
fn i6_remote_events_are_the_panes_own_and_reconcile_leaves_them() {
    let Some(mut server) = Server::start() else {
        return;
    };
    server.start_daemon();
    let pane = server.new_pane();
    let pane_pid: u64 = server
        .tmux(&["display", "-p", "-t", &pane, "#{pane_pid}"])
        .parse()
        .unwrap();
    let state = |server: &Server| server.tmux(&["show", "-pqv", "-t", &pane, "@agent_state"]);
    // agentd remote is the pane's process: no agent on the way.
    let chain = json!([[pane_pid, "agentd", 0]]);
    remote_hook(&server, &pane, "claude", chain.clone(), ev("SessionStart"));
    remote_hook(
        &server,
        &pane,
        "claude",
        chain.clone(),
        ev("UserPromptSubmit"),
    );
    assert_eq!(state(&server), "working");
    // A local agent on the way ran it: not the pane's.
    let run_by_agent = json!([[1, "agentd", 0], [2, "claude", 0], [pane_pid, "zsh", 0]]);
    remote_hook(&server, &pane, "claude", run_by_agent, ev("Stop"));
    assert_eq!(state(&server), "working");
    // Codex's facts are on the other host.
    remote_hook(&server, &pane, "codex", chain, ev("Stop"));
    assert_eq!(state(&server), "working");
    // Reconcile finds no local agent, and leaves a remote pane alone...
    server.tmux(&["set", "-p", "-t", &pane, "@agent_remote", "box:api"]);
    assert!(server.ctl(&["reconcile", &pane]).status.success());
    assert_eq!(state(&server), "working");
    // ...until agentd remote has left it.
    server.tmux(&["set", "-pu", "-t", &pane, "@agent_remote"]);
    assert!(server.ctl(&["reconcile", &pane]).status.success());
    assert_eq!(server.pane_options(&pane), Vec::<String>::new());
    let log = fs::read_to_string(server.runtime.join("state/tmux-agents/events.log")).unwrap();
    assert!(
        log.contains("UserPromptSubmit remote ready->working"),
        "{log}"
    );
    assert!(log.contains("ignored:remote-codex"), "{log}");
    assert!(log.contains("reconcile remote"), "{log}");
}
