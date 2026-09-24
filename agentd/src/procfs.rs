//! What agentd reads from /proc.

use std::fs;

/// The fields of /proc/<pid>/stat agentd uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub pid: u32,
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    /// The Unix session.
    pub session: u32,
    pub starttime: u64,
}

/// Parses /proc/<pid>/stat. The name may contain spaces and parentheses, so
/// the fields after it start at the last ") ".
pub fn parse_stat(pid: u32, text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(") ")?;
    let comm = text.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = text[close + 2..].split_whitespace().collect();
    Some(Stat {
        pid,
        comm,
        state: rest.first()?.chars().next()?,
        ppid: rest.get(1)?.parse().ok()?,
        session: rest.get(3)?.parse().ok()?,
        starttime: rest.get(19)?.parse().ok()?,
    })
}

/// Every process: one pass over /proc.
pub fn all() -> Vec<Stat> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(stat)
        .collect()
}

/// NUL-separated /proc file (environ, cmdline) as its entries.
pub fn entries(pid: u32, file: &str) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/{file}")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|e| !e.is_empty())
            .map(|e| String::from_utf8_lossy(e).into_owned())
            .collect(),
    )
}

pub fn stat(pid: u32) -> Option<Stat> {
    parse_stat(pid, &fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// `pid` and its ancestors, at most `max` of them, stopping before pid 1.
pub fn chain(pid: u32, max: usize) -> Vec<Stat> {
    let mut out = Vec::new();
    let mut pid = pid;
    while out.len() < max && pid > 1 {
        let Some(s) = stat(pid) else { break };
        pid = s.ppid;
        out.push(s);
    }
    out
}

/// Whether `ancestor` is `pid` or one of its parents, looking at no more than
/// `max` processes (as `ag_descends_from` in agent-lib.sh, with 8).
pub fn descends_from(pid: u32, ancestor: u32, max: usize) -> bool {
    let mut pid = pid;
    for _ in 0..max {
        if pid == ancestor {
            return true;
        }
        match stat(pid) {
            Some(s) if s.ppid > 1 => pid = s.ppid,
            _ => return false,
        }
    }
    false
}

/// The children of `pid`. /proc/<pid>/task/<tid>/children lists them per
/// thread (a Node or Rust agent spawns from several); without it, every
/// process's parent is looked up.
pub fn children(pid: u32) -> Vec<u32> {
    if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
        let mut found = Vec::new();
        let mut any = false;
        for task in tasks.flatten() {
            if let Ok(list) = fs::read_to_string(task.path().join("children")) {
                any = true;
                found.extend(
                    list.split_whitespace()
                        .filter_map(|p| p.parse::<u32>().ok()),
                );
            }
        }
        if any {
            return found;
        }
    }
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|p| stat(*p).is_some_and(|s| s.ppid == pid))
        .collect()
}

/// Children of `parent` whose command line contains `needle`, like
/// `pgrep -c -P <parent> -f <needle>` (B1).
pub fn count_children_matching(parent: u32, needle: &str) -> u32 {
    children(parent)
        .into_iter()
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
                String::from_utf8_lossy(&c)
                    .replace('\0', " ")
                    .contains(needle)
            })
        })
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_odd_names() {
        let s = parse_stat(
            7,
            "7 (a) b (c) S 1 7 7 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 12345 0 0",
        )
        .unwrap();
        assert_eq!(
            (s.comm.as_str(), s.state, s.ppid, s.session, s.starttime),
            ("a) b (c", 'S', 1, 7, 12345)
        );
        assert!(parse_stat(7, "7 (x) S 1").is_none());
    }

    #[test]
    fn own_chain() {
        let me = std::process::id();
        let chain = chain(me, 16);
        assert_eq!(chain[0].pid, me);
        assert_eq!(chain[1].pid, std::os::unix::process::parent_id());
        assert!(chain.iter().all(|s| s.pid > 1));
        assert!(descends_from(me, std::os::unix::process::parent_id(), 8));
        assert!(!descends_from(std::os::unix::process::parent_id(), me, 8));
    }

    #[test]
    fn children_and_command_lines() {
        // The marker as $0 of a shell (sleep would reject it and exit).
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 5", "shell-snapshots/snapshot-test"])
            .spawn()
            .unwrap();
        let me = std::process::id();
        assert!(children(me).contains(&child.id()));
        // Until it has exec'd, the child still shows our command line.
        for _ in 0..200 {
            let cmdline = fs::read(format!("/proc/{}/cmdline", child.id())).unwrap_or_default();
            if String::from_utf8_lossy(&cmdline).contains("snapshot-test") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(count_children_matching(me, "shell-snapshots/snapshot-"), 1);
        assert_eq!(count_children_matching(me, "no-such-marker"), 0);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
