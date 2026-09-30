//! `agentd-server`: what a server runs for remote panes, and nothing else
//! (design.md, "Remote panes"). The desktop's `agentd` does the same with
//! these two subcommands; this binary leaves out the daemon, the bar,
//! notifications and D-Bus, so it stays small.

use std::path::Path;
use std::process::ExitCode;
use std::time::SystemTime;

use agentd_common::event::Kind;
use agentd_common::{hook, identity, remote};

const USAGE: &str = "\
usage: agentd-server hold [name]           join the shell <name>, starting it if needed;
                                           without a name, list the held shells
       agentd-server hook claude|codex     an agent hook, in a held shell (else nothing)";

fn main() -> ExitCode {
    // Lossy: a hook must exit 0 whatever it gets (contract I2).
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["hook", rest @ ..] => {
            let started = SystemTime::now();
            let payload = hook::read_payload();
            let kind = match rest.first() {
                Some(&"claude") => Kind::Claude,
                Some(&"codex") => Kind::Codex,
                _ => return ExitCode::SUCCESS,
            };
            if let Some(hold) = hook::holder()
                && !identity::off()
            {
                hook::held(kind, &payload, started, Path::new(&hold));
            }
            ExitCode::SUCCESS
        }
        ["hold"] => remote::hold::list(),
        ["hold", "--serve", name] if remote::valid_name(name) => remote::hold::serve(name),
        ["hold", "--exec", pts, "--", program @ ..] if !program.is_empty() => {
            let program: Vec<String> = program.iter().map(|s| s.to_string()).collect();
            remote::hold::exec(pts, &program)
        }
        ["hold", name] if !name.starts_with('-') && remote::valid_name(name) => {
            remote::hold::attach(name)
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
