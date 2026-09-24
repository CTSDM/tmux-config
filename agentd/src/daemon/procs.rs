//! The /proc view X7 and B1 count over: the agents' trees, walked through
//! each thread's children (the cost follows the agents, not the machine),
//! then environments and command lines read only where needed.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use crate::core::codex::{ProcInfo, Procs};
use crate::procfs;

/// Whether this kernel lists each thread's children (CONFIG_PROC_CHILDREN).
pub fn children_files() -> bool {
    static YES: OnceLock<bool> = OnceLock::new();
    *YES.get_or_init(|| {
        let me = std::process::id();
        Path::new(&format!("/proc/{me}/task/{me}/children")).exists()
    })
}

pub struct ProcView {
    /// The pids walked from, or `None` for a scan of every process.
    roots: Option<HashSet<u32>>,
    /// Live processes, zombies left out.
    table: HashMap<u32, ProcInfo>,
    /// Every process seen, zombies too.
    any: HashMap<u32, ProcInfo>,
    environ: RefCell<HashMap<u32, Option<Vec<String>>>>,
    argv: RefCell<HashMap<u32, Option<Vec<String>>>>,
}

impl ProcView {
    /// Every process: only where children files don't exist, and then off
    /// the runtime's thread.
    pub fn scan() -> ProcView {
        let mut table = HashMap::new();
        let mut any = HashMap::new();
        for s in procfs::all() {
            let info = ProcInfo {
                ppid: s.ppid,
                sid: s.session,
                start: s.starttime,
            };
            any.insert(s.pid, info);
            if s.state != 'Z' {
                table.insert(s.pid, info);
            }
        }
        ProcView {
            roots: None,
            table,
            any,
            environ: RefCell::default(),
            argv: RefCell::default(),
        }
    }

    /// `roots` (agents, members of known command trees) and all their
    /// descendants. Parent links inside are those of a full scan, so X7 and
    /// B1 see the same trees; members reparented away count as roots.
    pub fn around(roots: impl IntoIterator<Item = u32>) -> ProcView {
        let roots: HashSet<u32> = roots.into_iter().collect();
        let mut table = HashMap::new();
        let mut any = HashMap::new();
        let mut todo: Vec<u32> = roots.iter().copied().collect();
        let mut seen = HashSet::new();
        while let Some(pid) = todo.pop() {
            if !seen.insert(pid) {
                continue;
            }
            let Some(s) = procfs::stat(pid) else { continue };
            let info = ProcInfo {
                ppid: s.ppid,
                sid: s.session,
                start: s.starttime,
            };
            any.insert(pid, info);
            if s.state == 'Z' {
                continue;
            }
            table.insert(pid, info);
            todo.extend(procfs::children(pid));
        }
        ProcView {
            roots: Some(roots),
            table,
            any,
            environ: RefCell::default(),
            argv: RefCell::default(),
        }
    }

    /// B1: children of `parent` whose command line contains `needle` (as
    /// `pgrep -c -P <parent> -f <needle>`).
    pub fn children_matching(&self, parent: u32, needle: &str) -> u32 {
        self.table
            .iter()
            .filter(|(_, p)| p.ppid == parent)
            .filter(|(pid, _)| {
                self.argv(**pid)
                    .is_some_and(|a| a.join(" ").contains(needle))
            })
            .count() as u32
    }

    /// Whether `pid` was walked from (a full scan covers everything).
    pub fn covers(&self, pid: u32) -> bool {
        self.roots.as_ref().is_none_or(|r| r.contains(&pid))
    }

    /// Start time of a live process.
    pub fn start_of(&self, pid: u32) -> Option<u64> {
        self.table.get(&pid).map(|p| p.start)
    }
}

impl Procs for ProcView {
    fn table(&self) -> &HashMap<u32, ProcInfo> {
        &self.table
    }
    fn stat(&self, pid: u32) -> Option<ProcInfo> {
        self.any.get(&pid).copied()
    }
    fn environ(&self, pid: u32) -> Option<Vec<String>> {
        self.environ
            .borrow_mut()
            .entry(pid)
            .or_insert_with(|| procfs::entries(pid, "environ"))
            .clone()
    }
    fn argv(&self, pid: u32) -> Option<Vec<String>> {
        self.argv
            .borrow_mut()
            .entry(pid)
            .or_insert_with(|| procfs::entries(pid, "cmdline"))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The processes at or below `root` in a view, with their parents.
    fn subtree(v: &ProcView, root: u32) -> Vec<(u32, u32)> {
        let below = |mut pid: u32| {
            while let Some(p) = v.table().get(&pid) {
                if pid == root {
                    return true;
                }
                pid = p.ppid;
            }
            false
        };
        let mut t: Vec<(u32, u32)> = v
            .table()
            .iter()
            .filter(|(pid, _)| below(**pid))
            .map(|(pid, p)| (*pid, p.ppid))
            .collect();
        t.sort_unstable();
        t
    }

    #[test]
    fn t3_6_around_sees_the_same_tree_as_a_scan() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 5 & sleep 5; :"])
            .spawn()
            .unwrap();
        let me = std::process::id();
        let root = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while subtree(&ProcView::around([me]), root).len() < 3
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let scan = ProcView::scan();
        let around = ProcView::around([me]);
        assert!(children_files());
        assert_eq!(subtree(&around, root).len(), 3, "sh and its two sleeps");
        assert_eq!(subtree(&around, root), subtree(&scan, root));
        assert!(around.table().len() < scan.table().len());
        assert_eq!(around.children_matching(root, "sleep"), 2);
        assert!(around.covers(me) && !around.covers(1) && scan.covers(1));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn sees_itself() {
        let view = ProcView::scan();
        let me = std::process::id();
        assert!(view.start_of(me).is_some());
        assert!(view.argv(me).is_some_and(|a| !a.is_empty()));
        assert!(view.environ(me).is_some());
        assert_eq!(view.table()[&me].ppid, std::os::unix::process::parent_id());
    }
}
