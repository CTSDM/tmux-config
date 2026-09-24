//! The /proc view Codex's X7 counts over: one scan, then environments and
//! command lines read only for the processes that need them.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::core::codex::{ProcInfo, Procs};
use crate::procfs;

pub struct ProcView {
    /// Live processes, zombies left out.
    table: HashMap<u32, ProcInfo>,
    /// Every process seen, zombies too.
    any: HashMap<u32, ProcInfo>,
    environ: RefCell<HashMap<u32, Option<Vec<String>>>>,
    argv: RefCell<HashMap<u32, Option<Vec<String>>>>,
}

impl ProcView {
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
            table,
            any,
            environ: RefCell::default(),
            argv: RefCell::default(),
        }
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
