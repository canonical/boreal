use clap::{Args, CommandFactory, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "boreal", about = "Mock Boreal shell commands")]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Action {
    /// List available workers.
    Workers,
    /// Start a workflow.
    Start {
        #[arg(value_parser = ["greet", "count"])]
        worker: String,
        id: String,
        name: String,
        #[arg(value_parser = clap::value_parser!(u64).range(1..=7200))]
        seconds: Option<u64>,
        #[command(flatten)]
        target: Target,
    },
    /// List workflows.
    List(Target),
    /// Describe a workflow.
    Status {
        id: String,
        #[command(flatten)]
        target: Target,
    },
    /// Show a workflow result.
    Result {
        id: String,
        #[command(flatten)]
        target: Target,
    },
    /// Cancel a workflow.
    Cancel {
        id: String,
        #[command(flatten)]
        target: Target,
    },
    /// Show a user's permissions.
    Permissions {
        #[arg(default_value = "alice", value_parser = ["alice", "bob"])]
        user: String,
    },
    /// Exit the shell.
    Exit,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub struct Target {
    #[arg(long, default_value = "alice", value_parser = ["alice", "bob"])]
    owner: String,
}

pub fn cli() -> clap::Command {
    Cli::command()
}

pub enum Error {
    Quote,
    Clap(clap::Error),
}

pub fn parse(line: &str) -> Result<Option<Action>, Error> {
    let words = shlex::split(line).ok_or(Error::Quote)?;
    if words.is_empty() {
        return Ok(None);
    }
    Cli::try_parse_from(std::iter::once("boreal").chain(words.iter().map(String::as_str)))
        .map(|cli| Some(cli.action))
        .map_err(Error::Clap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parses_commands_and_quoted_arguments() {
        let cases = [
            ("workers", Action::Workers),
            (
                "list --owner bob",
                Action::List(Target {
                    owner: "bob".into(),
                }),
            ),
            (
                "status run-1",
                Action::Status {
                    id: "run-1".into(),
                    target: Target {
                        owner: "alice".into(),
                    },
                },
            ),
            (
                "result run-1 --owner bob",
                Action::Result {
                    id: "run-1".into(),
                    target: Target {
                        owner: "bob".into(),
                    },
                },
            ),
            (
                "cancel run-1",
                Action::Cancel {
                    id: "run-1".into(),
                    target: Target {
                        owner: "alice".into(),
                    },
                },
            ),
            (
                "permissions",
                Action::Permissions {
                    user: "alice".into(),
                },
            ),
            (
                "permissions bob",
                Action::Permissions { user: "bob".into() },
            ),
            ("exit", Action::Exit),
            (
                "start count run-2 Élan",
                Action::Start {
                    worker: "count".into(),
                    id: "run-2".into(),
                    name: "Élan".into(),
                    seconds: None,
                    target: Target {
                        owner: "alice".into(),
                    },
                },
            ),
            (
                "start greet run-1 'Alice Smith' 5 --owner bob",
                Action::Start {
                    worker: "greet".into(),
                    id: "run-1".into(),
                    name: "Alice Smith".into(),
                    seconds: Some(5),
                    target: Target {
                        owner: "bob".into(),
                    },
                },
            ),
        ];
        for (line, expected) in cases {
            assert!(
                matches!(parse(line), Ok(Some(action)) if action == expected),
                "{line}"
            );
        }
        assert!(matches!(parse(" \t"), Ok(None)));
    }

    #[test]
    fn test_rejects_invalid_commands_without_executing_them() {
        for line in [
            "unknown",
            "/bin/sh",
            "start greet run-1 Alice 0",
            "start greet run-1 Alice 7201",
            "start other run-1 Alice",
            "status run-1 --owner eve",
            "workers; exit",
        ] {
            assert!(matches!(parse(line), Err(Error::Clap(_))), "{line}");
        }
        assert!(matches!(
            parse("start greet run-1 'unfinished"),
            Err(Error::Quote)
        ));
        assert!(
            matches!(parse("help"), Err(Error::Clap(err)) if err.kind() == clap::error::ErrorKind::DisplayHelp)
        );
    }

    #[test]
    fn test_shell_metacharacters_are_only_arguments() {
        let parsed = parse("start greet run-1 '$(not-executed)' 5");
        assert!(
            matches!(parsed, Ok(Some(Action::Start { name, .. })) if name == "$(not-executed)")
        );
    }
}
