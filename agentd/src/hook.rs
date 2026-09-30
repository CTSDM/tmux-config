//! `agentd hook claude|codex` (contract §1): reads the event, keeps what the
//! contract uses, and hands it to the daemon with the parent chain. Never
//! prints, always exits 0.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::Value;

pub use agentd_common::event::event_from_json;
use agentd_common::hook::{held, holder, links, read_payload, request};

use crate::client;
use crate::core::Kind;
use crate::identity;
use crate::parked;
use crate::procfs;
use crate::proto::{HookRequest, Parked, Request};

/// Parent chain looked at for I5 (as I4's).
const MAX_CHAIN: usize = 16;
/// Bash's hook in the checkout this binary was built from: where the rollback
/// switch sends the events (`@agents_bin` would cost a tmux call). If the
/// checkout moved, the usual place of the config.
const BASH_HOOK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../agents/bin/agent-hook");
const BASH_HOOK_USUAL: &str = ".config/tmux/agents/bin/agent-hook";
/// How long to wait for a daemon that has to be started first.
const START_BUDGET: Duration = Duration::from_millis(300);
/// The ack comes after one tmux read and one write. A daemon stuck longer
/// (tmux not answering) delays the agent at most this; reconciliation
/// repairs what is lost.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

pub fn run(kind: Option<Kind>) {
    let started = SystemTime::now();
    let payload = read_payload();
    // I1: exactly claude or codex, inside tmux; I5: or a parked Claude
    // session; I6: or held by `agentd hold`, for a remote pane.
    let Some(kind) = kind else {
        return;
    };
    if let Some(hold) = holder() {
        if !identity::off() {
            held(kind, &payload, started, Path::new(&hold));
        }
        return;
    }
    let (tmux, pane, parked) = match (
        env::var_os("TMUX").filter(|v| !v.is_empty()),
        env::var("TMUX_PANE").ok().filter(|v| !v.is_empty()),
    ) {
        (Some(tmux), Some(pane)) => (tmux, pane, None),
        _ if kind == Kind::Claude
            && env::var("CLAUDE_CODE_SESSION_KIND").as_deref() == Ok("bg") =>
        {
            let Some((viewer, parked)) = parked() else {
                return;
            };
            (viewer.tmux.into(), viewer.pane, Some(parked))
        }
        _ => return,
    };
    if identity::off() {
        // Bash's hook has no I5: it would drop the event anyway.
        if parked.is_none() {
            bash(kind, &payload);
        }
        return;
    }
    // jq reads invalid UTF-8 as U+FFFD; so do we.
    let Ok(json) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&payload)) else {
        return;
    };
    let Some(event) = event_from_json(&json) else {
        return;
    };
    let request = Request::Hook(Box::new(HookRequest {
        parked,
        ..request(kind, pane, event, started)
    }));
    let Some(paths) = client::paths(&tmux) else {
        return;
    };
    // No daemon after the budget: give up, reconciliation repairs it.
    if let Some(stream) = client::connect_or_start(&paths, START_BUDGET) {
        let _ = client::call(stream, &request, REPLY_TIMEOUT);
    }
}

/// I5: the pane that shows this parked session, from Claude Code's registry.
fn parked() -> Option<(parked::Viewer, Parked)> {
    let config = parked::config_dir(env::var("CLAUDE_CONFIG_DIR").ok())?;
    let chain: Vec<u32> = procfs::chain(std::os::unix::process::parent_id(), MAX_CHAIN)
        .iter()
        .map(|s| s.pid)
        .collect();
    let (agent, job) = parked::background(&config, &chain)?;
    let viewer = parked::viewer(&config, &job)?;
    let parked = Parked {
        agent,
        viewer: links(viewer.pid),
    };
    Some((viewer, parked))
}

/// The rollback switch: the event goes to bash's `agent-hook`, as it came.
fn bash(kind: Kind, payload: &[u8]) {
    let usual = env::var_os("HOME").map(|h| Path::new(&h).join(BASH_HOOK_USUAL));
    let Some(hook) = [Some(PathBuf::from(BASH_HOOK)), usual]
        .into_iter()
        .flatten()
        .find(|p| p.exists())
    else {
        crate::debug::log(&format!(
            "agentd.off: no bash agent-hook at {BASH_HOOK} or ~/{BASH_HOOK_USUAL}; event dropped"
        ));
        return;
    };
    let Ok(mut child) = Command::new(hook)
        .arg(kind.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload);
    }
    let _ = child.wait();
}
