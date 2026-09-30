use std::process::ExitCode;

use agentd::cli::{self, Command, Hold};
use agentd::remote;

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
        Some(Command::Remote { host, name, dir }) => match name {
            Some(name) => remote::attach::run(&host, &name, dir.as_deref()),
            None => remote::attach::list(&host),
        },
        Some(Command::Hold(hold)) => match hold {
            Hold::List => remote::hold::list(),
            Hold::Attach(name) if remote::valid_name(&name) => remote::hold::attach(&name),
            Hold::Serve(name) if remote::valid_name(&name) => remote::hold::serve(&name),
            Hold::Exec { pts, program } => remote::hold::exec(&pts, &program),
            _ => {
                eprintln!("agentd hold: a name is letters, digits, '.', '_' and '-'");
                ExitCode::from(2)
            }
        },
        Some(Command::Ctl { command, args }) => agentd::ctl::ctl(&command, &args),
        None => {
            eprintln!("{}", cli::USAGE);
            ExitCode::from(2)
        }
    }
}
