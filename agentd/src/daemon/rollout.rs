//! Reading a Codex rollout on from where it was left (X5): by file identity
//! and offset, never consuming an unfinished last line.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;

use crate::core::codex::{FileId, Rollout};

/// The rollout at `path`, read on from `r`. A missing file changes nothing:
/// unknown is never completion.
pub fn read_on(path: &str, mut r: Rollout) -> Rollout {
    if path.is_empty() {
        return r;
    }
    let Ok(file) = File::open(path) else { return r };
    let Ok(meta) = file.metadata() else { return r };
    let id = FileId {
        path: path.to_string(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    if r.must_restart(&id, meta.len()) {
        r.restart(id);
    }
    let mut reader = BufReader::new(file);
    if reader.seek(SeekFrom::Start(r.offset)).is_err() {
        return r;
    }
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(n) if n > 0 && line.ends_with(b"\n") => {
                r.offset += n as u64;
                if let Ok(record) = serde_json::from_slice(&line) {
                    r.apply(&record);
                }
            }
            _ => break,
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn lifecycle(kind: &str, turn: &str) -> String {
        format!(
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"{kind}\",\"turn_id\":\"{turn}\"}}}}\n"
        )
    }

    #[test]
    fn x5_partial_line_then_rotation() {
        let dir = std::env::temp_dir().join(format!("agentd-rollout-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        let p = path.to_str().unwrap();
        let end = lifecycle("task_complete", "one");
        fs::write(&path, lifecycle("task_started", "one") + &end[..15]).unwrap();
        let r = read_on(p, Rollout::default());
        assert_eq!(r.status, "busy");
        let offset = r.offset;
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&end.as_bytes()[15..])
            .unwrap();
        let r = read_on(p, r);
        assert_eq!(r.status, "complete");
        assert!(r.offset > offset);
        // A new file at the same path.
        let new = dir.join("rollout.new");
        fs::write(&new, lifecycle("task_started", "two")).unwrap();
        fs::rename(&new, &path).unwrap();
        let r = read_on(p, r);
        assert_eq!(
            (r.turn.as_deref(), r.status.as_str()),
            (Some("two"), "busy")
        );
        // Truncated in place.
        fs::write(&path, "").unwrap();
        let r = read_on(p, r);
        assert_eq!((r.offset, r.turn.clone()), (0, None));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn x5_missing_rollout_changes_nothing() {
        let r = Rollout {
            status: "busy".into(),
            ..Rollout::default()
        };
        assert_eq!(read_on("/nonexistent/agentd/rollout.jsonl", r.clone()), r);
        assert_eq!(read_on("", r.clone()), r);
    }
}
