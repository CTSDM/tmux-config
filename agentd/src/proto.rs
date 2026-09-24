//! Hook and ctl → daemon protocol (design.md, "Hook → daemon protocol"): a
//! Unix stream socket, one connection per call, one JSON line each way.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::core::{Event, Kind};

pub const VERSION: u32 = 1;

/// One process of the hook's parent chain: pid, comm, start time (clock
/// ticks since boot, field 22 of /proc/<pid>/stat).
pub type Link = (u32, String, u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookRequest {
    pub v: u32,
    pub kind: Kind,
    pub pane: String,
    pub event: Event,
    /// From the hook's parent upwards (I4).
    pub chain: Vec<Link>,
    /// The agent's variables the contract uses (`CLAUDE_CONFIG_DIR`).
    pub env: BTreeMap<String, String>,
    /// Epoch milliseconds when the hook started.
    pub t: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CtlRequest {
    pub v: u32,
    pub ctl: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Request {
    Hook(Box<HookRequest>),
    Ctl(CtlRequest),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Reply {
    pub fn ok() -> Self {
        Reply {
            ok: true,
            error: None,
            data: None,
        }
    }
    pub fn data(data: serde_json::Value) -> Self {
        Reply {
            data: Some(data),
            ..Reply::ok()
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Reply {
            ok: false,
            error: Some(message.into()),
            data: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_tell_hook_from_ctl() {
        let hook = r#"{"v":1,"kind":"claude","pane":"%1","event":{"hook_event_name":"Stop"},"chain":[[10,"claude",5]],"env":{},"t":1}"#;
        match serde_json::from_str::<Request>(hook).unwrap() {
            Request::Hook(h) => {
                assert_eq!(h.event.ev, "Stop");
                assert_eq!(h.chain, vec![(10, "claude".to_string(), 5)]);
            }
            other => panic!("{other:?}"),
        }
        let ctl = r#"{"v":1,"ctl":"status"}"#;
        assert!(
            matches!(serde_json::from_str::<Request>(ctl).unwrap(), Request::Ctl(c) if c.ctl == "status")
        );
    }
}
