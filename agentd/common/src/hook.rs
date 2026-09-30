//! The hook's side of a held shell (I6): the event goes to the holder named
//! in `AGENTD_HOLD`, which checks I4 and passes it on (remote/hold.rs).

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::event::{Event, Kind, event_from_json};
use crate::procfs;
use crate::proto::{HookRequest, Link, VERSION};
use crate::remote::{self, frame};

/// A bigger payload is cut here; the fields we keep are short anyway.
pub const MAX_PAYLOAD: u64 = 64 << 20;
/// Parent chain sent for I4 (which looks at 13).
const MAX_CHAIN: usize = 16;
/// The holder acks at once; a stuck one delays the agent at most this.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// The payload on stdin, read whatever happens so the agent's write never
/// fails.
pub fn read_payload() -> Vec<u8> {
    let mut payload = Vec::new();
    let _ = io::stdin()
        .lock()
        .take(MAX_PAYLOAD)
        .read_to_end(&mut payload);
    payload
}

/// The holder of this held shell, when the hook runs in one (and not in a
/// tmux started inside it).
pub fn holder() -> Option<OsString> {
    if env::var_os("TMUX").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    env::var_os(remote::HOLD_VAR).filter(|v| !v.is_empty())
}

/// The request for an event, with the hook's parent chain.
pub fn request(kind: Kind, pane: String, event: Event, started: SystemTime) -> HookRequest {
    let mut agent_env = BTreeMap::new();
    if let Ok(dir) = env::var("CLAUDE_CONFIG_DIR") {
        agent_env.insert("CLAUDE_CONFIG_DIR".to_string(), dir);
    }
    HookRequest {
        v: VERSION,
        kind,
        pane,
        event,
        chain: links(std::os::unix::process::parent_id()),
        env: agent_env,
        t: started
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        parked: None,
        remote: None,
    }
}

/// I6: a held agent's event goes to its holder, which checks I4 against the
/// held program and passes it to the remote pane (remote/hold.rs). No pane
/// here: the local end knows it.
pub fn held(kind: Kind, payload: &[u8], started: SystemTime, hold: &Path) {
    let Ok(json) = serde_json::from_str::<Value>(&String::from_utf8_lossy(payload)) else {
        return;
    };
    let Some(event) = event_from_json(&json) else {
        return;
    };
    let Ok(body) = serde_json::to_vec(&request(kind, String::new(), event, started)) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect(hold) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(REPLY_TIMEOUT));
    let _ = stream.set_read_timeout(Some(REPLY_TIMEOUT));
    if frame::write(&mut stream, frame::HOOK, &body).is_ok() {
        let _ = frame::read(&mut stream);
    }
}

/// `pid` and its parents, as I4 reads them.
pub fn links(pid: u32) -> Vec<Link> {
    procfs::chain(pid, MAX_CHAIN)
        .into_iter()
        .map(|s| (s.pid, s.comm, s.starttime))
        .collect()
}
