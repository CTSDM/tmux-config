//! agentd: the resident agent layer for tmux, replacing the hot path of the
//! bash scripts in `agents/bin` (docs/daemon/design.md). Behavior to preserve
//! is in docs/daemon/contract.md.

pub mod cli;
pub mod client;
pub mod core;
pub mod ctl;
pub mod daemon;
pub mod debug;
pub mod hook;
pub mod parked;
pub mod remote;

pub use agentd_common::{identity, procfs, proto};
