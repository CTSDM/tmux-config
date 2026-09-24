//! The state file (design.md, "State file"): the daemon's own bookkeeping,
//! so a restarted daemon keeps rounds, subagents and reminders. It belongs to
//! one tmux server instance: a restarted server reuses the socket path, and
//! so the file name, but its pane ids start again at %0.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::State;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedReminder {
    /// Epoch milliseconds when it fires.
    pub due_ms: u64,
    /// The `@agent_since` of the `needs` it belongs to.
    pub since: i64,
}

/// A tmux server instance: its pid and its start time (/proc/<pid>/stat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub pid: u32,
    pub start: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub v: u32,
    pub server: Option<Server>,
    pub core: State,
    pub reminders: BTreeMap<String, SavedReminder>,
}

/// The saved state of `server`. A missing or unreadable file, or one of
/// another server instance, is a fresh start.
pub fn load(path: &Path, server: Server) -> Saved {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Saved>(&b).ok())
        .filter(|saved| saved.server == Some(server))
        .unwrap_or_default()
}

/// Written whole and renamed into place.
pub fn save(path: &Path, saved: &Saved) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(saved)?)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: Server = Server {
        pid: 4242,
        start: 777,
    };

    #[test]
    fn round_trip_and_bad_files() {
        let dir = std::env::temp_dir().join(format!("agentd-store-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        assert_eq!(load(&path, SERVER), Saved::default());
        let mut saved = Saved {
            v: 1,
            server: Some(SERVER),
            ..Saved::default()
        };
        saved
            .core
            .rounds
            .entry("work".into())
            .or_default()
            .insert("%1".into());
        saved.reminders.insert(
            "%1".into(),
            SavedReminder {
                due_ms: 5,
                since: 3,
            },
        );
        save(&path, &saved).unwrap();
        assert_eq!(load(&path, SERVER), saved);
        // Another instance behind the same socket path: its state is not ours.
        let restarted = Server {
            pid: 4242,
            start: 778,
        };
        assert_eq!(load(&path, restarted), Saved::default());
        let other_pid = Server { pid: 99, ..SERVER };
        assert_eq!(load(&path, other_pid), Saved::default());
        // A file without an owner (older daemon) neither.
        save(
            &path,
            &Saved {
                server: None,
                ..saved.clone()
            },
        )
        .unwrap();
        assert_eq!(load(&path, SERVER), Saved::default());
        fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path, SERVER), Saved::default());
        fs::remove_dir_all(&dir).unwrap();
    }
}
