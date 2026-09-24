//! The command line: one binary, four subcommands (design.md, "Shape").

pub const USAGE: &str = "\
usage: agentd hook claude|codex     the agent hook: event JSON on stdin
       agentd daemon                one per tmux server, runs until it is gone
       agentd ensure                start the daemon of this tmux server unless it runs
       agentd ctl <command> [args]  a request to the daemon: seen <pane>, reconcile [panes],
                                    blink, blink-demo <session> <window> [secs], status, stop";

pub use crate::core::Kind;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `None` when the kind is not exactly `claude` or `codex`: the hook then
    /// does nothing, quietly (contract I1).
    Hook(Option<Kind>),
    Daemon,
    Ensure,
    Ctl {
        command: String,
        args: Vec<String>,
    },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Hook(_) => "hook",
            Command::Daemon => "daemon",
            Command::Ensure => "ensure",
            Command::Ctl { .. } => "ctl",
        }
    }
}

/// Parses the arguments after the program name. `None` means: print
/// [`USAGE`] and exit 2. A hook never fails here, whatever its arguments,
/// since it must always exit 0 (contract I2).
pub fn parse(args: &[String]) -> Option<Command> {
    let (first, rest) = args.split_first()?;
    match first.as_str() {
        "hook" => Some(Command::Hook(match rest.first().map(String::as_str) {
            Some("claude") => Some(Kind::Claude),
            Some("codex") => Some(Kind::Codex),
            _ => None,
        })),
        "daemon" if rest.is_empty() => Some(Command::Daemon),
        "ensure" if rest.is_empty() => Some(Command::Ensure),
        "ctl" => {
            let (command, args) = rest.split_first()?;
            Some(Command::Ctl {
                command: command.clone(),
                args: args.to_vec(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Option<Command> {
        parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn hook_kinds() {
        assert_eq!(
            parse_str(&["hook", "claude"]),
            Some(Command::Hook(Some(Kind::Claude)))
        );
        assert_eq!(
            parse_str(&["hook", "codex"]),
            Some(Command::Hook(Some(Kind::Codex)))
        );
    }

    #[test]
    fn hook_never_fails_to_parse() {
        assert_eq!(parse_str(&["hook"]), Some(Command::Hook(None)));
        assert_eq!(parse_str(&["hook", "Claude"]), Some(Command::Hook(None)));
        assert_eq!(parse_str(&["hook", "gemini"]), Some(Command::Hook(None)));
        // Extra arguments (bash had `codex --observe`) are ignored.
        assert_eq!(
            parse_str(&["hook", "codex", "--observe"]),
            Some(Command::Hook(Some(Kind::Codex)))
        );
    }

    #[test]
    fn daemon_and_ensure_take_no_arguments() {
        assert_eq!(parse_str(&["daemon"]), Some(Command::Daemon));
        assert_eq!(parse_str(&["ensure"]), Some(Command::Ensure));
        assert_eq!(parse_str(&["daemon", "x"]), None);
        assert_eq!(parse_str(&["ensure", "x"]), None);
    }

    #[test]
    fn ctl_needs_a_command() {
        assert_eq!(parse_str(&["ctl"]), None);
        assert_eq!(
            parse_str(&["ctl", "blink-demo", "api", "@3", "5"]),
            Some(Command::Ctl {
                command: "blink-demo".into(),
                args: vec!["api".into(), "@3".into(), "5".into()],
            })
        );
    }

    #[test]
    fn unknown_or_missing_subcommand() {
        assert_eq!(parse_str(&[]), None);
        assert_eq!(parse_str(&["start"]), None);
    }
}
