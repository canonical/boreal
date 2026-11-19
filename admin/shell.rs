mod policy;

use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;

use boreal_common::os;
use boreal_shell::completion::Grammar;
use boreal_shell::terminal::{self, Host, Invocation, Session, Submission};
use clap::builder::{PossibleValue, PossibleValuesParser};
use clap::error::ErrorKind;
use clap::{Arg, Command as ClapCommand};

use crate::policy::AdminPolicy;

const SUDO_BIN: &str = "/run/wrappers/bin/sudo";

#[derive(Clone, Debug, PartialEq, Eq)]
enum AdminAction {
    Services,
    Service { op: String, unit: String },
    UpdaterUpdate,
    UpdaterSchedule,
    UpdaterRecover,
    UpdaterStatus,
    Poweroff,
    Exit,
}

struct AdminGrammar {
    policy: AdminPolicy,
}

impl AdminGrammar {
    fn cli(&self) -> ClapCommand {
        let mut unit_values = Vec::new();
        for svc in &self.policy.services {
            unit_values.push(PossibleValue::new(svc.clone()));
            if let Some(short) = svc.strip_suffix(".service") {
                unit_values.push(PossibleValue::new(short.to_owned()));
            }
        }
        let op_parser = PossibleValuesParser::new(["status", "start", "stop", "restart"]);
        let unit_parser = PossibleValuesParser::new(unit_values);

        let mut root = ClapCommand::new("boreal-admin")
            .about("Restricted Boreal administration shell")
            .subcommand_required(true)
            .subcommand(ClapCommand::new("services").about("List managed Temporal services"))
            .subcommand(
                ClapCommand::new("service")
                    .about("Inspect or control an allowlisted Temporal service")
                    .arg(Arg::new("op").required(true).value_parser(op_parser))
                    .arg(Arg::new("unit").required(true).value_parser(unit_parser)),
            );

        if self.policy.updater_enabled {
            root = root.subcommand(
                ClapCommand::new("updater")
                    .about("Sync /etc/nixos, prebuild, schedule switch, or inspect update state")
                    .subcommand_required(true)
                    .subcommand(
                        ClapCommand::new("update")
                            .about("Sync /etc/nixos with remote branch and build generation"),
                    )
                    .subcommand(
                        ClapCommand::new("schedule").about(
                            "Schedule switch to built generation when no pipeline is running",
                        ),
                    )
                    .subcommand(
                        ClapCommand::new("recover").about("Recover from an interrupted update"),
                    )
                    .subcommand(ClapCommand::new("status").about("Show updater state")),
            );
        }

        if self.policy.allow_poweroff {
            root = root.subcommand(ClapCommand::new("poweroff").about("Power off this VM guest"));
        }

        root.subcommand(ClapCommand::new("exit").about("Exit the admin shell"))
    }

    fn parse(&self, line: &str) -> Result<Option<AdminAction>, ParseErr> {
        let words = shlex::split(line).ok_or(ParseErr::Quote)?;
        if words.is_empty() {
            return Ok(None);
        }
        let mut cmd = self.cli();
        let matches = cmd
            .try_get_matches_from_mut(
                std::iter::once("boreal-admin").chain(words.iter().map(String::as_str)),
            )
            .map_err(ParseErr::Clap)?;

        let action = match matches.subcommand() {
            Some(("services", _)) => AdminAction::Services,
            Some(("service", sub)) => {
                let op = sub.get_one::<String>("op").unwrap().clone();
                let raw_unit = sub.get_one::<String>("unit").unwrap();
                let unit = self
                    .policy
                    .resolve_service(raw_unit)
                    .map_err(|e| ParseErr::Clap(cmd.error(ErrorKind::InvalidValue, e)))?
                    .to_owned();
                AdminAction::Service { op, unit }
            }
            Some(("updater", sub)) => match sub.subcommand() {
                Some(("update", _)) => AdminAction::UpdaterUpdate,
                Some(("schedule", _)) => AdminAction::UpdaterSchedule,
                Some(("recover", _)) => AdminAction::UpdaterRecover,
                Some(("status", _)) => AdminAction::UpdaterStatus,
                _ => unreachable!(),
            },
            Some(("poweroff", _)) => AdminAction::Poweroff,
            Some(("exit", _)) => AdminAction::Exit,
            _ => unreachable!(),
        };
        Ok(Some(action))
    }

    fn to_ctl_args(action: &AdminAction) -> Option<Vec<String>> {
        match action {
            AdminAction::Exit => None,
            AdminAction::Services => Some(vec!["services".into()]),
            AdminAction::Service { op, unit } => {
                Some(vec!["service".into(), op.clone(), unit.clone()])
            }
            AdminAction::UpdaterUpdate => Some(vec!["updater".into(), "update".into()]),
            AdminAction::UpdaterSchedule => Some(vec!["updater".into(), "schedule".into()]),
            AdminAction::UpdaterRecover => Some(vec!["updater".into(), "recover".into()]),
            AdminAction::UpdaterStatus => Some(vec!["updater".into(), "status".into()]),
            AdminAction::Poweroff => Some(vec!["poweroff".into()]),
        }
    }

    fn execute(&self, action: &AdminAction) -> Result<(), String> {
        let Some(ctl_args) = Self::to_ctl_args(action) else {
            return Ok(());
        };
        let status = Command::new(SUDO_BIN)
            .arg("-n")
            .arg(&self.policy.ctl_bin)
            .args(&ctl_args)
            .status()
            .map_err(|e| format!("invoke sudo {}: {e}", self.policy.ctl_bin.display()))?;
        if !status.success() {
            return Err(format!("command failed ({status})"));
        }
        Ok(())
    }
}

enum ParseErr {
    Quote,
    Clap(clap::Error),
}

impl Grammar for AdminGrammar {
    fn cli(&self, _previous: &[String]) -> ClapCommand {
        self.cli()
    }
}

fn load_policy() -> Result<AdminPolicy, String> {
    if !Path::new(policy::DEFAULT_ADMIN_POLICY).exists()
        && !terminal::is_ssh_session()
        && let Ok(test_path) = env::var("BOREAL_TEST_ADMIN_POLICY")
    {
        return AdminPolicy::load_from_path(Path::new(&test_path), None);
    }
    AdminPolicy::load_system()
}

fn run_single_command(grammar: &AdminGrammar, raw: &str) -> Result<(), String> {
    match grammar.parse(raw) {
        Ok(Some(AdminAction::Exit)) | Ok(None) => Err("empty or unsupported SSH command".into()),
        Ok(Some(action)) => grammar.execute(&action),
        Err(ParseErr::Quote) => Err("unfinished quote or escape in command".into()),
        Err(ParseErr::Clap(err)) => Err(err.to_string()),
    }
}

fn run_interactive(grammar: Arc<AdminGrammar>) -> Result<(), Box<dyn Error>> {
    terminal::require_tty("boreal-admin")?;
    println!("Boreal restricted admin shell. Type help for permitted commands.");

    let uid = os::current_euid();
    let identity = os::user_by_uid(uid).map_err(|err| err.detail());
    let home = if !Path::new(policy::DEFAULT_ADMIN_POLICY).exists()
        && !terminal::is_ssh_session()
        && let Some(test_home) = env::var_os("BOREAL_TEST_HOME")
    {
        Ok(PathBuf::from(test_home))
    } else {
        identity.as_ref().map(|user| user.home().to_owned())
    };
    Session {
        program: "boreal-admin",
        caller: identity.as_ref().map_or("admin", |user| user.name()),
        host: Host::live_system(),
        home: home.as_deref().map_err(|err| err.as_str()),
        history: None,
    }
    .run(
        Arc::clone(&grammar),
        || None,
        |line| match grammar.parse(line) {
            Ok(Some(AdminAction::Exit)) => Ok(Submission::Exit),
            Ok(Some(action)) => grammar
                .execute(&action)
                .map(|()| Submission::Continue(true))
                .map_err(terminal::Error::Command),
            Ok(None) => Ok(Submission::Continue(true)),
            Err(ParseErr::Quote) => Err(terminal::Error::Quote),
            Err(ParseErr::Clap(err)) => Err(terminal::Error::Clap(err)),
        },
    )
}

fn run() -> Result<(), Box<dyn Error>> {
    let grammar = Arc::new(AdminGrammar {
        policy: load_policy()?,
    });
    match Invocation::from_env()? {
        Invocation::Interactive => run_interactive(grammar),
        Invocation::Command(command) => {
            run_single_command(&grammar, &command)?;
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("boreal-admin: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_grammar() -> AdminGrammar {
        AdminGrammar {
            policy: AdminPolicy::from_yaml(
                "\
services:
- temporal.service
- boreal-worker-alice-greet.service
updaterEnabled: true
updaterBin: /nix/store/x-updater/bin/boreal-updater
allowPoweroff: false
",
            )
            .unwrap(),
        }
    }

    #[test]
    fn test_admin_grammar_accepts_permitted_commands_and_rejects_escapes() {
        let g = sample_grammar();
        assert_eq!(
            g.parse("services").ok().flatten(),
            Some(AdminAction::Services)
        );
        assert_eq!(
            g.parse("service restart temporal").ok().flatten(),
            Some(AdminAction::Service {
                op: "restart".into(),
                unit: "temporal.service".into()
            })
        );
        assert_eq!(
            g.parse("updater update").ok().flatten(),
            Some(AdminAction::UpdaterUpdate)
        );
        assert_eq!(
            g.parse("updater schedule").ok().flatten(),
            Some(AdminAction::UpdaterSchedule)
        );
        assert_eq!(
            g.parse("updater status").ok().flatten(),
            Some(AdminAction::UpdaterStatus)
        );
        assert_eq!(
            g.parse("updater recover").ok().flatten(),
            Some(AdminAction::UpdaterRecover)
        );

        let complete = |line: &str| {
            boreal_shell::completion::candidates(&g, line, line.len())
                .into_iter()
                .map(|s| s.display_value().to_owned())
                .collect::<Vec<_>>()
        };
        let suggestions = complete("updater ");
        for expected in ["update", "schedule", "status", "recover"] {
            assert!(suggestions.iter().any(|s| s == expected), "{suggestions:?}");
        }
        let mut units = complete("service restart temp");
        units.sort_unstable();
        assert_eq!(units, ["temporal", "temporal.service"]);
        assert!(complete("pow").is_empty());
        assert!(complete("ru").is_empty());
        assert!(complete("sec").is_empty());

        for rejected in [
            "/bin/sh",
            "cat /home/alice/.config/boreal/secrets/tok",
            "sftp-server",
            "run greet",
            "secrets list",
            "service restart sshd",
            "service status postgresql",
            "services; id",
            "updater run",
            "updater update extra",
            "updater schedule 0123456789abcdef0123456789abcdef01234567",
            "update status",
            "poweroff",
            "sudo -l",
        ] {
            assert!(g.parse(rejected).is_err(), "{rejected}");
        }
    }
}
