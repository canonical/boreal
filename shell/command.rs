use std::collections::BTreeMap;

use boreal_common::catalog::{self, Catalog};
use boreal_common::secret;
use boreal_common::wire::RunInput;
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
        workflow: Option<String>,
        id: String,
        input: RunInput,
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
    Prompt(Missing),
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Missing {
    Run(Launch),
    Target {
        kind: Target,
        owner: Option<String>,
        id: Option<String>,
    },
    Permissions,
    Secret {
        remove: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub owner: Option<String>,
    pub worker: Option<String>,
    pub workflow: Option<String>,
    pub id: Option<String>,
    pub input: RunInput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    List,
    Status,
    Result,
    View,
    Cancel,
}

impl Target {
    pub fn action(self, owner: String, id: Option<String>) -> Option<Action> {
        Some(match self {
            Self::List => Action::List { owner },
            Self::Status => Action::Status { owner, id: id? },
            Self::Result => Action::Result { owner, id: id? },
            Self::View => Action::View { owner, id: id? },
            Self::Cancel => Action::Cancel { owner, id: id? },
        })
    }
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

    pub fn run_package(&self, owner: &str, worker: &str) -> Result<&catalog::Package, String> {
        let (_, package) = self
            .catalog
            .worker_and_package(owner, worker)
            .map_err(|e| e.detail())?;
        if !self.catalog.can_write(&self.caller, owner, worker) {
            return Err(format!(
                "permission denied: '{}' cannot run '{owner}/{worker}'",
                self.caller
            ));
        }
        Ok(package)
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
                    .and_then(catalog::Package::only_workflow)
                    .map_or("", |workflow| workflow.description());
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
            .help("Registered worker name")
            .value_parser(PossibleValuesParser::new(worker_values));

        let run_cmd = Command::new("run")
            .about("Run a workflow")
            .arg(worker_arg)
            .arg(
                Arg::new("legacy-id")
                    .value_name("ID")
                    .help("Workflow ID (positional shorthand)"),
            )
            .arg(
                Arg::new("id")
                    .long("id")
                    .value_name("ID")
                    .help("Workflow ID; generated when omitted"),
            )
            .arg(
                Arg::new("args")
                    .num_args(0..)
                    .help("Typed workflow arguments"),
            )
            .arg(
                Arg::new("input-json")
                    .long("input-json")
                    .value_name("JSON")
                    .conflicts_with("args")
                    .help("JSON input, or - to read stdin in a non-interactive command"),
            )
            .arg(
                Arg::new("workflow")
                    .long("workflow")
                    .value_name("ALIAS")
                    .help("Exported workflow alias"),
            )
            .arg(owner_arg(&writable, &self.caller));

        let list_cmd = Command::new("list")
            .about("List workflows")
            .arg(owner_arg(&readable, &self.caller));

        let status_cmd = Command::new("status")
            .about("Describe a workflow")
            .arg(Arg::new("id").help("Workflow ID"))
            .arg(owner_arg(&readable, &self.caller));

        let result_cmd = Command::new("result")
            .about("Show a workflow result")
            .arg(Arg::new("id").help("Workflow ID"))
            .arg(owner_arg(&readable, &self.caller));

        let view_cmd = Command::new("view")
            .about("Inspect a workflow run in a live TUI")
            .arg(Arg::new("id").help("Workflow ID"))
            .arg(owner_arg(&readable, &self.caller));

        let cancel_cmd = Command::new("cancel")
            .about("Cancel a workflow")
            .arg(Arg::new("id").help("Workflow ID"))
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
                    .arg(Arg::new("name").help("Secret file name")),
            )
            .subcommand(
                Command::new("remove")
                    .about("Delete a secret after confirmation")
                    .arg(Arg::new("name").help("Secret file name")),
            );

        Command::new("boreal")
            .about("Boreal lab shell")
            .subcommand_required(true)
            .arg(
                Arg::new("no-ask")
                    .long("no-ask")
                    .global(true)
                    .action(ArgAction::SetTrue)
                    .help("Fail instead of prompting for missing values"),
            )
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

    #[cfg(test)]
    pub fn parse(&self, line: &str) -> Result<Option<Action>, Error> {
        let words = shlex::split(line).ok_or(Error::Quote)?;
        self.parse_words(&words)
    }

    pub fn parse_interactive(&self, line: &str) -> Result<Option<Action>, Error> {
        let words = shlex::split(line).ok_or(Error::Quote)?;
        self.parse_mode(&words, true)
    }

    pub fn parse_words(&self, words: &[String]) -> Result<Option<Action>, Error> {
        self.parse_mode(words, false)
    }

    fn parse_mode(&self, words: &[String], interactive: bool) -> Result<Option<Action>, Error> {
        if words.is_empty() {
            return Ok(None);
        }
        let mut cmd = self.cli(None);
        let matches = cmd
            .try_get_matches_from_mut(
                std::iter::once("boreal").chain(words.iter().map(String::as_str)),
            )
            .map_err(Error::Clap)?;
        let ask = interactive && !matches.get_flag("no-ask");

        let action = match matches.subcommand() {
            Some(("workers", _)) => Action::Workers,
            Some(("run", sub)) => {
                let worker = sub.get_one::<String>("worker").cloned();
                let workflow = sub.get_one::<String>("workflow").cloned();
                let flag_id = sub.get_one::<String>("id");
                let positional = sub.get_one::<String>("legacy-id");
                let id = flag_id.or(positional).cloned();
                let owner = sub.get_one::<String>("owner").cloned();
                let mut args: Vec<String> = sub
                    .get_many::<String>("args")
                    .map_or_else(Vec::new, |vals| vals.cloned().collect());
                if flag_id.is_some()
                    && let Some(first) = positional
                {
                    args.insert(0, first.clone());
                }
                let input = match sub.get_one::<String>("input-json") {
                    Some(_) if !args.is_empty() => {
                        return Err(Error::Clap(cmd.error(
                            ErrorKind::ArgumentConflict,
                            "--input-json cannot be combined with positional workflow inputs",
                        )));
                    }
                    Some(raw) => RunInput::Json(raw.clone()),
                    None => RunInput::Positional(args),
                };

                self.prepare_run(
                    Launch {
                        owner,
                        worker,
                        workflow,
                        id,
                        input,
                    },
                    ask,
                )
                .map_err(|e| Error::Clap(cmd.error(ErrorKind::ValueValidation, e)))?
            }
            Some((name @ ("list" | "status" | "result" | "view" | "cancel"), sub)) => {
                let kind = match name {
                    "list" => Target::List,
                    "status" => Target::Status,
                    "result" => Target::Result,
                    "view" => Target::View,
                    _ => Target::Cancel,
                };
                let owner = sub.get_one::<String>("owner").cloned();
                let id = sub.try_get_one::<String>("id").ok().flatten().cloned();
                if let Some(id) = &id {
                    validate_id_or_clap(&mut cmd, id)?;
                }
                if let Some(action) = owner
                    .clone()
                    .and_then(|owner| kind.action(owner, id.clone()))
                {
                    action
                } else if ask {
                    Action::Prompt(Missing::Target { kind, owner, id })
                } else {
                    return Err(Error::Clap(cmd.error(
                        ErrorKind::ValueValidation,
                        if owner.is_none() {
                            "specify --owner"
                        } else {
                            "specify a workflow ID"
                        },
                    )));
                }
            }
            Some(("permissions", sub)) => match sub.get_one::<String>("user").cloned() {
                Some(user) => Action::Permissions { user },
                None if ask => Action::Prompt(Missing::Permissions),
                None => {
                    return Err(Error::Clap(
                        cmd.error(ErrorKind::ValueValidation, "specify an account"),
                    ));
                }
            },
            Some(("secrets", sub)) => match sub.subcommand() {
                Some(("list", _)) => Action::Secrets(SecretAction::List),
                Some((verb @ ("set" | "remove"), s)) => {
                    let remove = verb == "remove";
                    match s.get_one::<String>("name").cloned() {
                        Some(name) => named_secret(name, remove)
                            .map_err(|e| Error::Clap(cmd.error(ErrorKind::ValueValidation, e)))?,
                        None if ask => Action::Prompt(Missing::Secret { remove }),
                        None => {
                            return Err(Error::Clap(
                                cmd.error(ErrorKind::ValueValidation, "specify a secret name"),
                            ));
                        }
                    }
                }
                _ => unreachable!(),
            },
            Some(("exit", _)) => Action::Exit,
            _ => unreachable!(),
        };
        Ok(Some(action))
    }

    pub fn prepare_run(&self, launch: Launch, ask: bool) -> Result<Action, String> {
        if let Some(id) = &launch.id {
            catalog::validate_workflow_id(id).map_err(|e| e.detail())?;
        }
        let (Some(owner), Some(worker)) = (&launch.owner, &launch.worker) else {
            return if ask {
                Ok(Action::Prompt(Missing::Run(launch)))
            } else {
                Err(if launch.owner.is_none() {
                    "specify --owner"
                } else {
                    "specify a worker"
                }
                .into())
            };
        };
        let package = self.run_package(owner, worker)?;
        if launch.workflow.is_none() && package.only_workflow().is_none() && ask {
            return Ok(Action::Prompt(Missing::Run(launch)));
        }
        let workflow = package
            .select(launch.workflow.as_deref())
            .map_err(|e| e.detail())?;
        match &launch.input {
            RunInput::Positional(args) => {
                if ask
                    && (!workflow.accepts_positionals() && args.is_empty()
                        || args.len() < workflow.required_count()
                        || launch.id.is_none() && args.is_empty() && !workflow.args().is_empty())
                {
                    return Ok(Action::Prompt(Missing::Run(launch)));
                }
                workflow.validate_and_encode(args).map_err(|e| e.detail())?;
            }
            RunInput::Json(raw) if raw != "-" => {
                workflow.validate_json(raw).map_err(|e| e.detail())?;
            }
            RunInput::Json(_) => {}
        }
        Ok(Action::Run {
            owner: owner.to_owned(),
            worker: worker.to_owned(),
            workflow: launch.workflow,
            id: launch.id.unwrap_or_else(new_id),
            input: launch.input,
        })
    }
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn named_secret(name: String, remove: bool) -> Result<Action, String> {
    secret::validate_name(&name).map_err(|e| e.detail())?;
    Ok(Action::Secrets(if remove {
        SecretAction::Remove { name }
    } else {
        SecretAction::Set { name }
    }))
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
    fn test_missing_values_prompt_only_in_interactive_mode() {
        let ctx = Context::mock();
        let incomplete = ["run", "run greet", "status", "cancel", "secrets set"];
        for line in incomplete {
            assert!(
                matches!(ctx.parse_interactive(line), Ok(Some(Action::Prompt(_)))),
                "{line}"
            );
            assert!(ctx.parse(line).is_err(), "{line}");
        }
        assert!(ctx.parse_interactive("run greet --no-ask").is_err());
        assert!(
            matches!(ctx.parse_interactive("run greet --id job Alice 5"), Ok(Some(Action::Run { id, .. })) if id == "job")
        );
        assert!(
            matches!(ctx.parse(r#"run greet --input-json '{"name":"Alice"}'"#), Ok(Some(Action::Run { id, input: RunInput::Json(_), .. })) if uuid::Uuid::parse_str(&id).is_ok())
        );
    }

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
                    workflow: None,
                    id: "run-2".into(),
                    input: RunInput::Positional(vec!["Élan".into()]),
                    owner: "alice".into(),
                },
            ),
            (
                "run count run-2 Élan --workflow count",
                Action::Run {
                    worker: "count".into(),
                    workflow: Some("count".into()),
                    id: "run-2".into(),
                    input: RunInput::Positional(vec!["Élan".into()]),
                    owner: "alice".into(),
                },
            ),
            (
                "run greet run-1 'Alice Smith' 5 --owner bob",
                Action::Run {
                    worker: "greet".into(),
                    workflow: None,
                    id: "run-1".into(),
                    input: RunInput::Positional(vec!["Alice Smith".into(), "5".into()]),
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
            matches!(parsed, Ok(Some(Action::Run { input: RunInput::Positional(args), .. })) if args == vec!["$(not-executed)", "5"])
        );
    }
}
