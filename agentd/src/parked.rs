//! I5: parked sessions. Claude Code can send a session to the background
//! (a slash command, `claude --bg`): its own `claude daemon` then runs it,
//! without the pane's `TMUX` and `TMUX_PANE`, and the `claude` left in the
//! pane only shows it. Claude Code's session registry
//! (`<config>/sessions/<pid>.json`) says which: the background process has
//! `kind: "bg"` and a `jobId`; the pane's process, `kind: "interactive"`,
//! `parkedJobId` set to that job. Neither file is documented; seen on
//! Claude Code 2.1.285.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::procfs;

/// Where a parked session's events go: the pane, and its tmux server, as
/// the process that shows it has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    pub pid: u32,
    pub tmux: String,
    pub pane: String,
}

/// Claude Code's config directory, as Claude Code picks it.
pub fn config_dir(claude_config_dir: Option<String>) -> Option<PathBuf> {
    match claude_config_dir.filter(|d| !d.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(Path::new(&env::var_os("HOME")?).join(".claude")),
    }
}

/// One registry entry, when its process is still the one that wrote it
/// (same pid and start time).
fn entry(sessions: &Path, pid: u32) -> Option<Value> {
    let text = fs::read_to_string(sessions.join(format!("{pid}.json"))).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let start = procfs::stat(pid)?.starttime.to_string();
    (v.get("procStart").and_then(Value::as_str) == Some(start.as_str())).then_some(v)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// For a hook without a pane: the background session among its parents
/// (`chain`, pids from the hook's parent up), its pid and job.
pub fn background(config: &Path, chain: &[u32]) -> Option<(u32, String)> {
    let sessions = config.join("sessions");
    chain.iter().find_map(|&pid| {
        let v = entry(&sessions, pid)?;
        if str_of(&v, "kind")? != "bg" {
            return None;
        }
        Some((pid, str_of(&v, "jobId")?.to_string()))
    })
}

/// The live process that shows `job` in a pane, with that pane.
pub fn viewer(config: &Path, job: &str) -> Option<Viewer> {
    let sessions = config.join("sessions");
    fs::read_dir(&sessions).ok()?.flatten().find_map(|e| {
        let name = e.file_name();
        let pid: u32 = name.to_str()?.strip_suffix(".json")?.parse().ok()?;
        let v = entry(&sessions, pid)?;
        if str_of(&v, "kind")? != "interactive" || str_of(&v, "parkedJobId")? != job {
            return None;
        }
        let env = procfs::entries(pid, "environ")?;
        let var = |k: &str| {
            env.iter()
                .find_map(|e| e.strip_prefix(k)?.strip_prefix('='))
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        Some(Viewer {
            pid,
            tmux: var("TMUX")?,
            pane: var("TMUX_PANE")?,
        })
    })
}

/// For the pane's agent process (`claude`): the background session it
/// shows, when it parked one that still runs.
pub fn shown_by(config: &Path, viewer: u32) -> Option<u32> {
    let sessions = config.join("sessions");
    let job = str_of(&entry(&sessions, viewer)?, "parkedJobId")?.to_string();
    fs::read_dir(&sessions).ok()?.flatten().find_map(|e| {
        let pid: u32 = e
            .file_name()
            .to_str()?
            .strip_suffix(".json")?
            .parse()
            .ok()?;
        let v = entry(&sessions, pid)?;
        (str_of(&v, "kind")? == "bg" && str_of(&v, "jobId")? == job).then_some(pid)
    })
}

/// The `CLAUDE_CONFIG_DIR` a process started with.
pub fn config_dir_of(pid: u32) -> Option<PathBuf> {
    let env = procfs::entries(pid, "environ")?;
    config_dir(
        env.iter()
            .find_map(|e| e.strip_prefix("CLAUDE_CONFIG_DIR="))
            .map(str::to_string),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::id;

    fn registry(entries: &[Value]) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "agentd-parked-{}-{}",
            id(),
            entries.len() + entries.iter().map(|e| e.to_string().len()).sum::<usize>()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sessions")).unwrap();
        for e in entries {
            let pid = e["pid"].as_u64().unwrap();
            fs::write(
                dir.join("sessions").join(format!("{pid}.json")),
                e.to_string(),
            )
            .unwrap();
        }
        dir
    }

    fn start(pid: u32) -> String {
        procfs::stat(pid).unwrap().starttime.to_string()
    }

    #[test]
    fn i5_background_viewer_and_back() {
        // This test process plays the background session; a child with a
        // pane in its environment, the viewer.
        let me = id();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env("TMUX", "/tmp/tmux-test/default,1,0")
            .env("TMUX_PANE", "%9")
            .spawn()
            .unwrap();
        let viewer_pid = child.id();
        // Its start time, once exec'd.
        let mut viewer_start = None;
        for _ in 0..100 {
            if procfs::stat(viewer_pid).is_some_and(|s| s.comm == "sleep") {
                viewer_start = Some(start(viewer_pid));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let config = registry(&[
            json!({"pid": me, "procStart": start(me), "kind": "bg", "jobId": "job1"}),
            json!({"pid": viewer_pid, "procStart": viewer_start.unwrap(),
                   "kind": "interactive", "parkedJobId": "job1"}),
        ]);
        assert_eq!(
            background(&config, &[4_000_000, me]),
            Some((me, "job1".into()))
        );
        assert_eq!(
            background(&config, &[viewer_pid]),
            None,
            "interactive is no background"
        );
        assert_eq!(
            viewer(&config, "job1"),
            Some(Viewer {
                pid: viewer_pid,
                tmux: "/tmp/tmux-test/default,1,0".into(),
                pane: "%9".into()
            })
        );
        assert_eq!(viewer(&config, "other"), None);
        assert_eq!(shown_by(&config, viewer_pid), Some(me));
        let _ = child.kill();
        let _ = child.wait();
        // The viewer gone: nobody shows the job.
        assert_eq!(viewer(&config, "job1"), None);
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn i5_a_reused_pid_is_not_the_entry() {
        let me = id();
        let config =
            registry(&[json!({"pid": me, "procStart": "1", "kind": "bg", "jobId": "job1"})]);
        assert_eq!(background(&config, &[me]), None);
        let _ = fs::remove_dir_all(config);
    }
}
