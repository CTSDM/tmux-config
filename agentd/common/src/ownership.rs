//! Which process an event belongs to: I4 on the hook's own chain, I6 for a
//! remote pane's (docs/daemon/contract.md).

/// I4 with C4: the pane's own agent, from the hook's parent chain (pid and
/// comm, parent first). The walk must reach `pane_pid` within 13 processes,
/// and exactly one of them, `pane_pid` included, is `claude` or `codex`.
pub fn owner<'a>(chain: impl IntoIterator<Item = (u32, &'a str)>, pane_pid: u32) -> Option<u32> {
    let mut agent = None;
    let mut agents = 0;
    for (pid, comm) in chain.into_iter().take(13) {
        if comm == "claude" || comm == "codex" {
            agents += 1;
            agent = Some(pid);
        }
        if agents > 1 {
            return None;
        }
        if pid == pane_pid {
            return if agents == 1 { agent } else { None };
        }
    }
    None
}

/// I6: an event a remote pane's `agentd remote` passes on, from its own chain.
/// The walk must reach `pane_pid` within 13 processes, none of them an agent
/// (the agent runs on the other host).
pub fn remote_owner<'a>(chain: impl IntoIterator<Item = (u32, &'a str)>, pane_pid: u32) -> bool {
    for (pid, comm) in chain.into_iter().take(13) {
        if comm == "claude" || comm == "codex" {
            return false;
        }
        if pid == pane_pid {
            return true;
        }
    }
    false
}
