use std::collections::BTreeMap;

use boreal_common::catalog::{self, Catalog};
use boreal_common::secret;
use clap::builder::{PossibleValue, PossibleValuesParser};
use clap::error::ErrorKind;
use clap::{Arg, ArgAction, Command};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretAction {
    List,
    Set { name: String },
    Remove { name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Workers,
    Run {
        worker: String,
        id: String,
        args: Vec<String>,
        owner: String,
    },
    List {
        owner: String,
    },
    Status {
        id: String,
        owner: String,
    },
    Result {
        id: String,
        owner: String,
    },
    View {
        id: String,
        owner: String,
    },
    Cancel {
        id: String,
        owner: String,
    },
    Permissions {
        user: String,
    },
    Secrets(SecretAction),
    Exit,
}

#[derive(Clone)]
pub struct Context {
    catalog: Catalog,
    caller: String,
}

impl Context {
    pub fn new(catalog: Catalog, caller: impl Into<String>) -> Self {
        Self {
            catalog,
            caller: caller.into(),
        }
    }

    #[cfg(test)]
    pub fn mock() -> Self {
        Self::new(Catalog::mock(), "alice")
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn caller(&self) -> &str {
        &self.caller
    }

    pub fn cli(&self, run_owner_hint: Option<&str>) -> Command {
        let readable = self.catalog.readable_owners(&self.caller);
        let writable = self.catalog.writable_owners(&self.caller);
        let perm_users = self.catalog.visible_permission_users(&self.caller);

        let mut worker_choices: BTreeMap<String, String> = BTreeMap::new();
        for (owner, policy) in self.catalog.users() {
            if let Some(hint) = run_owner_hint
                && owner != hint
            {
                continue;
            }
            for (wname, worker) in policy.workers() {
                if !self.catalog.can_write(&self.caller, owner, wname) {
                    continue;
                }
                let desc = self
                    .catalog
                    .packages()
                    .get(worker.package())
                    .map_or("", |p| p.description());
                worker_choices
                    .entry(wname.clone())
                    .or_insert_with(|| desc.to_owned());
            }
        }

        let worker_values: Vec<PossibleValue> = worker_choices
            .into_iter()
            .map(|(name, desc)| {
                if desc.is_empty() {
                    PossibleValue::new(name)
                } else {
                    PossibleValue::new(name).help(desc)
                }
            })
            .collect();

        let worker_arg = Arg::new("worker")
            .required(true)
            .help("Registered worker name")
            .value_parser(PossibleValuesParser::new(worker_values));

        let run_cmd = Command::new("run")
            .about("Run a workflow")
            .arg(worker_arg)
            .arg(Arg::new("id").required(true).help("Workflow ID"))
            .arg(
                Arg::new("args")
                    .num_args(0..)
                    .help("Typed workflow arguments"),
            )
            .arg(owner_arg(&writable, &self.caller));

        let list_cmd = Command::new("list")
            .about("List workflows")
            .arg(owner_arg(&readable, &self.caller));

        let status_cmd = Command::new("status")
            .about("Describe a workflow")
            .arg(Arg::new("id").required(true).help("Workflow ID"))
            .arg(owner_arg(&readable, &self.caller));

        let result_cmd = Command::new("result")
            .about("Show a workflow result")
            .arg(Arg::new("id").required(true).help("Workflow ID"))
            .arg(owner_arg(&readable, &self.caller));

        let view_cmd = Command::new("view")
            .about("Inspect a workflow run in a live TUI")
            .arg(Arg::new("id").required(true).help("Workflow ID"))
            .arg(owner_arg(&readable, &self.caller));

        let cancel_cmd = Command::new("cancel")
            .about("Cancel a workflow")
            .arg(Arg::new("id").required(true).help("Workflow ID"))
            .arg(owner_arg(&writable, &self.caller));

        let mut perm_arg = Arg::new("user")
            .help("Boreal user to inspect")
            .value_parser(PossibleValuesParser::new(
                perm_users
                    .iter()
                    .map(|u| PossibleValue::new((*u).to_owned()))
                    .collect::<Vec<_>>(),
            ));
        if perm_users.contains(&self.caller.as_str()) {
            perm_arg = perm_arg.default_value(self.caller.clone());
        } else {
            perm_arg = perm_arg.required(true);
        }
        let perms_cmd = Command::new("permissions")
            .about("Show a user's permissions")
            .arg(perm_arg);

        let secrets_cmd = Command::new("secrets")
            .about("Manage user secrets in ~/.config/boreal/secrets")
            .subcommand_required(true)
            .subcommand(Command::new("list").about("List secret names and byte sizes"))
            .subcommand(
                Command::new("set")
                    .about("Prompt for a secret without echo (or read from SSH stdin)")
                    .arg(Arg::new("name").required(true).help("Secret file name")),
            )
            .subcommand(
                Command::new("remove")
                    .about("Delete a secret after confirmation")
                    .arg(Arg::new("name").required(true).help("Secret file name")),
            );

        Command::new("boreal")
            .about("Boreal lab shell")
            .subcommand_required(true)
            .subcommand(Command::new("workers").about("List available workers"))
            .subcommand(run_cmd)
            .subcommand(list_cmd)
            .subcommand(status_cmd)
            .subcommand(result_cmd)
            .subcommand(view_cmd)
            .subcommand(cancel_cmd)
            .subcommand(perms_cmd)
            .subcommand(secrets_cmd)
            .subcommand(Command::new("exit").about("Exit the shell"))
    }

    pub fn parse(&self, line: &str) -> Result<Option<Action>, Error> {
        let words = shlex::split(line).ok_or(Error::Quote)?;
        if words.is_empty() {
            return Ok(None);
        }
        let mut cmd = self.cli(None);
        let matches = cmd
            .try_get_matches_from_mut(
                std::iter::once("boreal").chain(words.iter().map(String::as_str)),
            )
            .map_err(Error::Clap)?;

        let action = match matches.subcommand() {
            Some(("workers", _)) => Action::Workers,
            Some(("run", sub)) => {
                let worker = sub.get_one::<String>("worker").unwrap().clone();
                let id = sub.get_one::<String>("id").unwrap().clone();
                let owner = sub.get_one::<String>("owner").unwrap().clone();
                let args: Vec<String> = sub
                    .get_many::<String>("args")
                    .map_or_else(Vec::new, |vals| vals.cloned().collect());

                if let Err(e) = catalog::validate_workflow_id(&id) {
                    return Err(Error::Clap(
                        cmd.error(ErrorKind::ValueValidation, e.detail()),
                    ));
                }
                let Ok((w, pkg)) = self.catalog.worker_and_package(&owner, &worker) else {
                    return Err(Error::Clap(cmd.error(
                        ErrorKind::InvalidValue,
                        format!("owner '{owner}' has no worker named '{worker}'"),
                    )));
                };
                if !self.catalog.can_write(&self.caller, &owner, w.name()) {
                    return Err(Error::Clap(cmd.error(
                        ErrorKind::InvalidValue,
                        format!(
                            "permission denied: '{}' cannot run '{owner}/{worker}'",
                            self.caller
                        ),
                    )));
                }
                if let Err(e) = pkg.validate_and_encode(&args) {
                    return Err(Error::Clap(
                        cmd.error(ErrorKind::ValueValidation, e.detail()),
                    ));
                }
                Action::Run {
                    worker,
                    id,
                    args,
                    owner,
                }
            }
            Some(("list", sub)) => Action::List {
                owner: sub.get_one::<String>("owner").unwrap().clone(),
            },
            Some(("status", sub)) => {
                let id = sub.get_one::<String>("id").unwrap().clone();
                validate_id_or_clap(&mut cmd, &id)?;
                Action::Status {
                    id,
                    owner: sub.get_one::<String>("owner").unwrap().clone(),
                }
            }
            Some(("result", sub)) => {
                let id = sub.get_one::<String>("id").unwrap().clone();
                validate_id_or_clap(&mut cmd, &id)?;
                Action::Result {
                    id,
                    owner: sub.get_one::<String>("owner").unwrap().clone(),
                }
            }
            Some(("view", sub)) => {
                let id = sub.get_one::<String>("id").unwrap().clone();
                validate_id_or_clap(&mut cmd, &id)?;
                Action::View {
                    id,
                    owner: sub.get_one::<String>("owner").unwrap().clone(),
                }
            }
            Some(("cancel", sub)) => {
                let id = sub.get_one::<String>("id").unwrap().clone();
                validate_id_or_clap(&mut cmd, &id)?;
                Action::Cancel {
                    id,
                    owner: sub.get_one::<String>("owner").unwrap().clone(),
                }
            }
            Some(("permissions", sub)) => Action::Permissions {
                user: sub.get_one::<String>("user").unwrap().clone(),
            },
            Some(("secrets", sub)) => match sub.subcommand() {
                Some(("list", _)) => Action::Secrets(SecretAction::List),
                Some(("set", s)) => {
                    let name = s.get_one::<String>("name").unwrap().clone();
                    if let Err(e) = secret::validate_name(&name) {
                        return Err(Error::Clap(
                            cmd.error(ErrorKind::ValueValidation, e.detail()),
                        ));
                    }
                    Action::Secrets(SecretAction::Set { name })
                }
                Some(("remove", s)) => {
                    let name = s.get_one::<String>("name").unwrap().clone();
                    if let Err(e) = secret::validate_name(&name) {
                        return Err(Error::Clap(
                            cmd.error(ErrorKind::ValueValidation, e.detail()),
                        ));
                    }
                    Action::Secrets(SecretAction::Remove { name })
                }
                _ => unreachable!(),
            },
            Some(("exit", _)) => Action::Exit,
            _ => unreachable!(),
        };
        Ok(Some(action))
    }
}

fn owner_arg(allowed: &[&str], caller: &str) -> Arg {
    let values: Vec<PossibleValue> = allowed
        .iter()
        .map(|u| PossibleValue::new((*u).to_owned()))
        .collect();
    let mut arg = Arg::new("owner")
        .long("owner")
        .action(ArgAction::Set)
        .help("Target Boreal account")
        .value_parser(PossibleValuesParser::new(values));
    if allowed.contains(&caller) {
        arg = arg.default_value(caller.to_owned());
    } else {
        arg = arg.required(true);
    }
    arg
}

fn validate_id_or_clap(cmd: &mut Command, id: &str) -> Result<(), Error> {
    catalog::validate_workflow_id(id)
        .map_err(|e| Error::Clap(cmd.error(ErrorKind::ValueValidation, e.detail())))
}

pub enum Error {
    Quote,
    Clap(clap::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parses_dynamic_commands_and_filters_by_permission() {
        let ctx = Context::mock();
        let cases = [
            ("workers", Action::Workers),
            (
                "list --owner bob",
                Action::List {
                    owner: "bob".into(),
                },
            ),
            (
                "status run-1",
                Action::Status {
                    id: "run-1".into(),
                    owner: "alice".into(),
                },
            ),
            (
                "result run-1 --owner bob",
                Action::Result {
                    id: "run-1".into(),
                    owner: "bob".into(),
                },
            ),
            (
                "view run-1 --owner bob",
                Action::View {
                    id: "run-1".into(),
                    owner: "bob".into(),
                },
            ),
            (
                "cancel run-1",
                Action::Cancel {
                    id: "run-1".into(),
                    owner: "alice".into(),
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
            ("secrets list", Action::Secrets(SecretAction::List)),
            (
                "secrets set api_token",
                Action::Secrets(SecretAction::Set {
                    name: "api_token".into(),
                }),
            ),
            (
                "secrets remove api_token",
                Action::Secrets(SecretAction::Remove {
                    name: "api_token".into(),
                }),
            ),
            ("exit", Action::Exit),
            (
                "run count run-2 Élan",
                Action::Run {
                    worker: "count".into(),
                    id: "run-2".into(),
                    args: vec!["Élan".into()],
                    owner: "alice".into(),
                },
            ),
            (
                "run greet run-1 'Alice Smith' 5 --owner bob",
                Action::Run {
                    worker: "greet".into(),
                    id: "run-1".into(),
                    args: vec!["Alice Smith".into(), "5".into()],
                    owner: "bob".into(),
                },
            ),
        ];
        for (line, expected) in cases {
            assert!(
                matches!(ctx.parse(line), Ok(Some(action)) if action == expected),
                "{line}"
            );
        }
        assert!(matches!(ctx.parse(" \t"), Ok(None)));

        // Alice does NOT have write on bob/count in mock catalog!
        assert!(matches!(
            ctx.parse("run count run-1 Alice --owner bob"),
            Err(Error::Clap(_))
        ));
    }

    #[test]
    fn test_rejects_invalid_commands_and_inline_secret_values() {
        let ctx = Context::mock();
        for line in [
            "unknown",
            "/bin/sh",
            "start greet run-1 Alice 5",
            "run greet run-1 Alice nope",
            "run other run-1 Alice",
            "status run-1 --owner eve",
            "secrets set api_token leaked_value",
            "secrets set ../escape",
            "workers; exit",
        ] {
            assert!(matches!(ctx.parse(line), Err(Error::Clap(_))), "{line}");
        }
        assert!(matches!(
            ctx.parse("run greet run-1 'unfinished"),
            Err(Error::Quote)
        ));
        assert!(
            matches!(ctx.parse("help"), Err(Error::Clap(err)) if err.kind() == ErrorKind::DisplayHelp)
        );
    }

    #[test]
    fn test_shell_metacharacters_are_only_arguments() {
        let ctx = Context::mock();
        let parsed = ctx.parse("run greet run-1 '$(not-executed)' 5");
        assert!(
            matches!(parsed, Ok(Some(Action::Run { args, .. })) if args == vec!["$(not-executed)", "5"])
        );
    }
}
