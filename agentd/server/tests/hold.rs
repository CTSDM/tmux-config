//! Remote panes, the server's end (design.md, "Remote panes"): `agentd-server
//! hold` spoken to in frames, as `agentd remote` does over ssh. No tmux: the held
//! program is a `/bin/sh`, and its agent a copy of it named `claude`.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};
use std::{env, fs};

use agentd_common::remote::frame::{
    self, ATTACHED, DETACHED, EVENT, EXIT, HELLO, HOOK, INPUT, OUTPUT,
};
use serde_json::{Value, json};

static DIRS: AtomicUsize = AtomicUsize::new(0);

/// A holder folder of its own, short (socket paths), with `claude` in it.
struct Lab {
    dir: PathBuf,
}

impl Lab {
    fn new() -> Lab {
        let n = DIRS.fetch_add(1, Ordering::SeqCst);
        let dir = PathBuf::from(format!("/tmp/agentd-rt-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::copy(fs::canonicalize("/bin/sh").unwrap(), dir.join("bin/claude")).unwrap();
        Lab { dir }
    }

    fn hold(&self) -> PathBuf {
        self.dir.join("hold")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agentd-server"));
        c.args(args)
            .env("AGENTD_HOLD_DIR", self.hold())
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.dir)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.join("bin").display()),
            )
            .env("ENV", "/dev/null")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        c
    }

    /// A client of the held shell `name`, as ssh runs it.
    fn attach(&self, name: &str, have: Option<u64>) -> Client {
        self.attach_in(name, have, None)
    }

    /// The same, asking for the shell to start in `dir`.
    fn attach_in(&self, name: &str, have: Option<u64>, dir: Option<&str>) -> Client {
        let mut child = self
            .command(&["hold", name])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut up = child.stdin.take().unwrap();
        let mut down = child.stdout.take().unwrap();
        let (tx, frames) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(Some(f)) = frame::read(&mut down) {
                if tx.send(f).is_err() {
                    return;
                }
            }
        });
        let hello =
            json!({"term": "xterm-256color", "rows": 24, "cols": 80, "have": have, "dir": dir});
        frame::write(&mut up, HELLO, hello.to_string().as_bytes()).unwrap();
        Client {
            child,
            up: Some(up),
            frames,
            shown: Vec::new(),
        }
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        // Holders still running (a failed test) end with their program.
        if let Ok(entries) = fs::read_dir(self.hold()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(name) = name
                    .strip_prefix("hold-")
                    .and_then(|n| n.strip_suffix(".sock"))
                {
                    let mut c = self.attach(name, None);
                    c.input("exit\n");
                    let _ = c.until(|k, _, _| k == EXIT);
                }
            }
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Client {
    child: Child,
    up: Option<ChildStdin>,
    frames: Receiver<(u8, Vec<u8>)>,
    /// All the output it got.
    shown: Vec<u8>,
}

impl Client {
    fn input(&mut self, keys: &str) {
        frame::write(self.up.as_mut().unwrap(), INPUT, keys.as_bytes()).unwrap();
    }

    /// Frames until `done` says so, given each frame and all the output so
    /// far; the last one.
    fn until(&mut self, mut done: impl FnMut(u8, &[u8], &[u8]) -> bool) -> Option<(u8, Vec<u8>)> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let (kind, payload) = self.frames.recv_timeout(left).ok()?;
            if kind == OUTPUT {
                self.shown.extend_from_slice(&payload);
            }
            if done(kind, &payload, &self.shown) {
                return Some((kind, payload));
            }
        }
        None
    }

    fn output_has(&mut self, text: &str) -> bool {
        let has = |shown: &[u8]| String::from_utf8_lossy(shown).contains(text);
        has(&self.shown) || self.until(|_, _, shown| has(shown)).is_some()
    }

    /// ssh goes: the holder stays.
    fn drop_connection(mut self) {
        self.up = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn json_of(payload: &[u8]) -> Value {
    serde_json::from_slice(payload).unwrap()
}

fn attached(client: &mut Client) -> Value {
    let (kind, payload) = client
        .until(|k, _, _| k == ATTACHED || k == DETACHED)
        .unwrap();
    assert_eq!(kind, ATTACHED, "{}", String::from_utf8_lossy(&payload));
    json_of(&payload)
}

/// Keys that make the held shell's `claude` run a hook after `delay`.
fn hook_keys(event: &str, delay: &str) -> String {
    let agentd = env!("CARGO_BIN_EXE_agentd-server");
    format!(
        "claude -c 'sleep {delay}; printf %s \"{{\\\"hook_event_name\\\":\\\"{event}\\\",\\\"session_id\\\":\\\"s1\\\"}}\" | {agentd} hook claude' &\n"
    )
}

#[test]
fn a_held_shell_outlives_its_client_and_keeps_what_it_missed() {
    let lab = Lab::new();
    let mut a = lab.attach("api", None);
    let hello = attached(&mut a);
    assert_eq!(hello, json!({"new": true, "at": 0}));
    a.input("echo marker-$((6*7))\n");
    assert!(a.output_has("marker-42"));
    // Its agent's hook comes while nobody is attached.
    a.input(&hook_keys("SessionStart", "0.5"));
    sleep(Duration::from_millis(100));
    let have = a.shown.len() as u64;
    a.drop_connection();
    sleep(Duration::from_millis(1000));

    let mut b = lab.attach("api", Some(have));
    let hello = attached(&mut b);
    assert_eq!(hello["new"], false);
    assert_eq!(hello["at"], have);
    // The kept event first, as the pane's (the local end names the pane).
    let (kind, event) = b.until(|k, _, _| k == EVENT || k == OUTPUT).unwrap();
    assert_eq!(kind, EVENT);
    let event = json_of(&event);
    assert_eq!(event["event"]["hook_event_name"], "SessionStart");
    assert_eq!(event["remote"]["name"], "api");
    assert!(event["remote"]["agent"].as_u64().unwrap() > 1, "{event}");
    assert_eq!(event["pane"], "");

    b.input("exit 3\n");
    let (_, code) = b.until(|k, _, _| k == EXIT).unwrap();
    assert_eq!(json_of(&code)["code"], 3);
    let _ = b.child.wait();
    assert!(!lab.hold().join("hold-api.sock").exists());
}

#[test]
fn a_new_pane_gets_the_screen_back() {
    let lab = Lab::new();
    let mut a = lab.attach("web", None);
    attached(&mut a);
    a.input("echo first-$((1+1))\n");
    assert!(a.output_has("first-2"));
    a.input(&hook_keys("SessionStart", "0"));
    let (_, live) = a.until(|k, _, _| k == EVENT).unwrap();
    assert_eq!(json_of(&live)["remote"].get("replay"), None);
    // Another pane takes it: the first is told, the second gets everything,
    // and the agent's events again, to show its state.
    let mut b = lab.attach("web", None);
    let (kind, why) = a.until(|k, _, _| k == DETACHED).unwrap();
    assert_eq!(kind, DETACHED);
    assert_eq!(json_of(&why)["why"], "attached somewhere else");
    let hello = attached(&mut b);
    assert_eq!(hello, json!({"new": false, "at": 0}));
    let (_, again) = b.until(|k, _, _| k == EVENT).unwrap();
    let again = json_of(&again);
    assert_eq!(again["event"]["hook_event_name"], "SessionStart");
    assert_eq!(again["remote"]["replay"], true);
    assert!(b.output_has("first-2"));
    // A reconnect of the same pane: no replay.
    let have = b.shown.len() as u64;
    b.drop_connection();
    let mut c = lab.attach("web", Some(have));
    attached(&mut c);
    c.input("echo third\n");
    assert!(c.output_has("third"));
    assert!(c.frames.try_iter().all(|(k, _)| k != EVENT));
    let list = lab.command(&["hold"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&list.stdout), "web\tattached\n");
    c.input("exit\n");
    c.until(|k, _, _| k == EXIT).unwrap();
}

#[test]
fn only_the_held_programs_agent_is_passed_on() {
    let lab = Lab::new();
    let mut a = lab.attach("ops", None);
    attached(&mut a);
    // Not run from the held shell: I4 against the held program says no.
    let mut hook = UnixStream::connect(lab.hold().join("hold-ops.sock")).unwrap();
    hook.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let request = json!({
        "v": 1, "kind": "claude", "pane": "", "event": {"hook_event_name": "Stop"},
        "chain": [[std::process::id(), "claude", 0]], "env": {}, "t": 0,
    });
    frame::write(&mut hook, HOOK, request.to_string().as_bytes()).unwrap();
    let (kind, reply) = frame::read(&mut hook).unwrap().unwrap();
    assert_eq!(kind, HOOK);
    assert_eq!(
        json_of(&reply),
        json!({"ok": false, "error": "not-its-agent"})
    );
    // From it: passed on while attached.
    a.input(&hook_keys("UserPromptSubmit", "0"));
    let (_, event) = a.until(|k, _, _| k == EVENT).unwrap();
    assert_eq!(
        json_of(&event)["event"]["hook_event_name"],
        "UserPromptSubmit"
    );
    a.input("exit\n");
    a.until(|k, _, _| k == EXIT).unwrap();
    let _ = a.up.take();
    let _ = a.child.wait();
    // The holder has gone with its program.
    let deadline = Instant::now() + Duration::from_secs(2);
    while lab.hold().join("hold-ops.sock").exists() && Instant::now() < deadline {
        sleep(Duration::from_millis(20));
    }
    assert!(!lab.hold().join("hold-ops.sock").exists());
}

#[test]
fn a_new_shell_starts_in_the_folder_asked_for() {
    let lab = Lab::new();
    fs::create_dir_all(lab.dir.join("src/api")).unwrap();
    for (name, dir, expect) in [
        ("in-dir", "~/src/api", lab.dir.join("src/api")),
        ("absolute", "/", PathBuf::from("/")),
        ("missing", "~/no/such/folder", lab.dir.clone()),
    ] {
        let mut c = lab.attach_in(name, None, Some(dir));
        attached(&mut c);
        c.input("echo \"at:$(pwd):\"\n");
        let want = format!("at:{}:", expect.display());
        assert!(
            c.output_has(&want),
            "{name}: {}",
            String::from_utf8_lossy(&c.shown)
        );
        // Only when it starts: attaching again elsewhere keeps where it is.
        c.input("exit\n");
        c.until(|k, _, _| k == EXIT).unwrap();
    }
}
