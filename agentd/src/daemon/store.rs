//! The state file (design.md, "State file"): the daemon's own bookkeeping,
//! so a restarted daemon keeps rounds, subagents and reminders.

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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub v: u32,
    pub core: State,
    pub reminders: BTreeMap<String, SavedReminder>,
}

/// A missing or unreadable file is a fresh start.
pub fn load(path: &Path) -> Saved {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
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

    #[test]
    fn round_trip_and_bad_files() {
        let dir = std::env::temp_dir().join(format!("agentd-store-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        assert_eq!(load(&path), Saved::default());
        let mut saved = Saved {
            v: 1,
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
        assert_eq!(load(&path), saved);
        fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), Saved::default());
        fs::remove_dir_all(&dir).unwrap();
    }
}
