//! Remote panes (design.md, "Remote panes"): the server's end and the
//! frames are in agentd-common, shared with agentd-server; the local end,
//! `agentd remote`, is here.

pub use agentd_common::remote::*;

pub mod attach;
