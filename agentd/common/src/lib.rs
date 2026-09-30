//! What the desktop's `agentd` and a server's `agentd-server` share
//! (design.md, "Remote panes"): hook events as the contract reads them, the
//! ownership rules, the protocol types, /proc, runtime paths, and the holder
//! of remote panes with the hook's side of it. No async runtime, no D-Bus.

pub mod event;
pub mod hook;
pub mod identity;
pub mod ownership;
pub mod procfs;
pub mod proto;
pub mod remote;
