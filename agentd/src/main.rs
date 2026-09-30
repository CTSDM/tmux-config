use std::process::ExitCode;

use agentd::cli::{self, Command};

fn main() -> ExitCode {
    // Lossy, not `env::args()`: that panics on arguments that are not UTF-8,
    // and a hook must exit 0 whatever it gets (contract I2).
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match cli::parse(&args) {
        Some(Command::Hook(kind)) => {
            agentd::hook::run(kind);
            ExitCode::SUCCESS
        }
        Some(Command::Daemon) => agentd::daemon::run(),
        Some(Command::Ensure) => agentd::ctl::ensure(),
        Some(Command::Bridge) => agentd::daemon::bridge(),
        Some(Command::Ctl { command, args }) => agentd::ctl::ctl(&command, &args),
        None => {
            eprintln!("{}", cli::USAGE);
            ExitCode::from(2)
        }
    }
}
