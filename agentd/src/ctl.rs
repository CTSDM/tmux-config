//! `agentd ensure` and `agentd ctl <command>`.

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;
use std::time::Duration;

use crate::client;
use crate::proto::{CtlRequest, Request, VERSION};

/// tmux.conf runs `ensure` on load; waiting a little lets it report failure.
const ENSURE_BUDGET: Duration = Duration::from_secs(2);
const CTL_TIMEOUT: Duration = Duration::from_secs(5);
/// Reconciling every pane of a big server takes a while.
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(30);

fn tmux_var() -> Option<OsString> {
    let tmux = env::var_os("TMUX").filter(|v| !v.is_empty());
    if tmux.is_none() {
        eprintln!("agentd: not inside tmux (TMUX is not set)");
    }
    tmux
}

/// Starts the daemon of this tmux server unless it runs. Idempotent.
pub fn ensure() -> ExitCode {
    let Some(paths) = tmux_var().and_then(|t| client::paths(&t)) else {
        return ExitCode::FAILURE;
    };
    if client::connect_or_start(&paths, ENSURE_BUDGET).is_some() {
        ExitCode::SUCCESS
    } else {
        eprintln!("agentd ensure: the daemon did not start");
        ExitCode::FAILURE
    }
}

pub fn ctl(command: &str, args: &[String]) -> ExitCode {
    let Some(tmux) = tmux_var() else {
        return ExitCode::FAILURE;
    };
    let Some(paths) = client::paths(&tmux) else {
        return ExitCode::FAILURE;
    };
    // Reconcile and blink run on config load, seen on focus, the blink demo
    // from prefix+Q: maybe before any hook started us.
    let (stream, timeout) = match command {
        "reconcile" | "seen" => (
            client::connect_or_start(&paths, ENSURE_BUDGET),
            RECONCILE_TIMEOUT,
        ),
        "blink" | "blink-demo" => (client::connect_or_start(&paths, ENSURE_BUDGET), CTL_TIMEOUT),
        _ => (
            std::os::unix::net::UnixStream::connect(&paths.socket).ok(),
            CTL_TIMEOUT,
        ),
    };
    let Some(stream) = stream else {
        eprintln!("agentd ctl: no daemon for this tmux server");
        return ExitCode::FAILURE;
    };
    let request = Request::Ctl(CtlRequest {
        v: VERSION,
        ctl: command.to_string(),
        args: args.to_vec(),
    });
    match client::call(stream, &request, timeout) {
        Ok(reply) if reply.ok => {
            if let Some(data) = reply.data {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).unwrap_or_default()
                );
            }
            ExitCode::SUCCESS
        }
        Ok(reply) => {
            eprintln!("agentd ctl {command}: {}", reply.error.unwrap_or_default());
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("agentd ctl {command}: {e}");
            ExitCode::FAILURE
        }
    }
}
