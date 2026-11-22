use boreal_common::catalog::{self, Catalog};
use boreal_common::secret;
use boreal_common::wire::RunInput;
use clap::builder::{PossibleValue, PossibleValuesParser};
use clap::error::ErrorKind;
use clap::{Arg, ArgAction, Command};
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};

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
    pub input_json: Option<String>,
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

    pub fn owners(&self, writable: bool) -> Vec<&str> {
        if writable {
            self.catalog.writable_owners(&self.caller)
        } else {
            self.catalog.readable_owners(&self.caller)
        }
    }

    fn target_owner(
        &self,
        explicit: Option<&str>,
        writable: bool,
    ) -> Result<Option<String>, String> {
        let owner = explicit.unwrap_or(self.caller());
        let allowed = if writable {
            self.catalog.has_any_write(self.caller(), owner)
        } else {
            self.catalog.has_any_read(self.caller(), owner)
        };
        if allowed {
            Ok(Some(owner.to_owned()))
        } else if explicit.is_some() {
            Err(format!(
                "permission denied: account '{owner}' is not available for this command"
            ))
        } else {
            Ok(None)
        }
    }

    pub fn cli(&self) -> Command {
        let readable = self.catalog.readable_owners(&self.caller);
        let perm_users = self.catalog.visible_permission_users(&self.caller);

        let mut worker_choices = Vec::new();
        for (owner, policy) in self.catalog.users() {
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
                if owner == self.caller() {
                    worker_choices.push((wname.clone(), desc.to_owned()));
                }
                worker_choices.push((format!("{wname}/{owner}"), desc.to_owned()));
            }
        }

        let worker_arg = Arg::new("worker")
            .value_name("WORKER[/OWNER]")
            .help("Registered worker; append /OWNER to select another account")
            .add(ArgValueCandidates::new(move || {
                worker_choices
                    .iter()
                    .map(|(value, desc)| {
                        CompletionCandidate::new(value.clone())
                            .help((!desc.is_empty()).then(|| desc.clone().into()))
                    })
                    .collect()
            }));

        let run_cmd = Command::new("run")
            .about("Run a workflow")
            .arg(worker_arg)
            .arg(
                Arg::new("id")
                    .long("id")
                    .value_name("ID")
                    .help("Workflow ID; generated when omitted"),
            )
            .arg(
                Arg::new("input-json")
                    .long("input-json")
                    .value_name("JSON")
                    .help("JSON input, or - to read stdin in a non-interactive command"),
            )
            .arg(
                Arg::new("workflow")
                    .long("workflow")
                    .value_name("ALIAS")
                    .help("Exported workflow alias"),
            );

        let list_cmd = Command::new("list").about("List workflows").arg(
            Arg::new("owner")
                .value_name("OWNER")
                .help("Target account; defaults to your account")
                .value_parser(PossibleValuesParser::new(
                    readable.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                )),
        );
        let execution_arg = || {
            Arg::new("id")
                .value_name("ID[/OWNER]")
                .help("Workflow ID; append /OWNER to select another account")
        };

        let status_cmd = Command::new("status")
            .about("Describe a workflow")
            .arg(execution_arg());

        let result_cmd = Command::new("result")
            .about("Show a workflow result")
            .arg(execution_arg());

        let view_cmd = Command::new("view")
            .about("Inspect a workflow run in a live TUI")
            .arg(execution_arg());

        let cancel_cmd = Command::new("cancel")
            .about("Cancel a workflow")
            .arg(execution_arg());

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
        let mut cmd = self.cli();
        let matches = cmd
            .try_get_matches_from_mut(
                std::iter::once("boreal").chain(words.iter().map(String::as_str)),
            )
            .map_err(Error::Clap)?;
        let ask = interactive && !matches.get_flag("no-ask");

        let action = match matches.subcommand() {
            Some(("workers", _)) => Action::Workers,
            Some(("run", sub)) => {
                let (worker, explicit) = parse_selector(&mut cmd, sub.get_one::<String>("worker"))?;
                let owner = self
                    .target_owner(explicit, true)
                    .map_err(|e| invalid(&mut cmd, e))?;
                self.prepare_run(
                    Launch {
                        owner,
                        worker: worker.map(str::to_owned),
                        workflow: sub.get_one::<String>("workflow").cloned(),
                        id: sub.get_one::<String>("id").cloned(),
                        input_json: sub.get_one::<String>("input-json").cloned(),
                    },
                    ask,
                )
                .map_err(|e| invalid(&mut cmd, e))?
            }
            Some((name @ ("list" | "status" | "result" | "view" | "cancel"), sub)) => {
                let kind = match name {
                    "list" => Target::List,
                    "status" => Target::Status,
                    "result" => Target::Result,
                    "view" => Target::View,
                    _ => Target::Cancel,
                };
                let (id, explicit) = if kind == Target::List {
                    (None, sub.get_one::<String>("owner").map(String::as_str))
                } else {
                    parse_selector(&mut cmd, sub.get_one::<String>("id"))?
                };
                if let Some(id) = id {
                    catalog::validate_workflow_id(id).map_err(|e| invalid(&mut cmd, e.detail()))?;
                }
                let id = id.map(str::to_owned);
                let owner = self
                    .target_owner(explicit, kind == Target::Cancel)
                    .map_err(|e| invalid(&mut cmd, e))?;
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
                            "specify an account"
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
                    "specify a worker as WORKER/OWNER"
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
        let input = match launch.input_json {
            Some(raw) => {
                if raw != "-" {
                    workflow.validate_json(&raw).map_err(|e| e.detail())?;
                }
                raw
            }
            None if ask => return Ok(Action::Prompt(Missing::Run(launch))),
            None => workflow
                .validate_and_encode(&[])
                .map_err(|_| "provide workflow input with --input-json")?,
        };
        Ok(Action::Run {
            owner: owner.to_owned(),
            worker: worker.to_owned(),
            workflow: launch.workflow,
            id: launch.id.unwrap_or_else(new_id),
            input: RunInput::Json(input),
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

pub fn split_selector(value: &str) -> Result<(&str, Option<&str>), &'static str> {
    match value.rsplit_once('/') {
        Some((target, owner)) if !target.is_empty() && !owner.is_empty() => {
            Ok((target, Some(owner)))
        }
        None if !value.is_empty() => Ok((value, None)),
        _ => Err("expected a non-empty selector and, after '/', a non-empty owner"),
    }
}

fn parse_selector<'a>(
    cmd: &mut Command,
    value: Option<&'a String>,
) -> Result<(Option<&'a str>, Option<&'a str>), Error> {
    match value {
        Some(value) => split_selector(value)
            .map(|(target, owner)| (Some(target), owner))
            .map_err(|e| invalid(cmd, e)),
        None => Ok((None, None)),
    }
}

fn invalid(cmd: &mut Command, message: impl std::fmt::Display) -> Error {
    Error::Clap(cmd.error(ErrorKind::ValueValidation, message))
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
            matches!(ctx.parse_interactive(r#"run greet --id job --input-json '{"name":"Alice","wait_seconds":5}'"#), Ok(Some(Action::Run { id, .. })) if id == "job")
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
                "list",
                Action::List {
                    owner: "alice".into(),
                },
            ),
            (
                "list bob",
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
                "result run-1/bob",
                Action::Result {
                    id: "run-1".into(),
                    owner: "bob".into(),
                },
            ),
            (
                "view run-1/bob",
                Action::View {
                    id: "run-1".into(),
                    owner: "bob".into(),
                },
            ),
            (
                "status suite/run-1/alice",
                Action::Status {
                    id: "suite/run-1".into(),
                    owner: "alice".into(),
                },
            ),
            (
                "cancel run-1/bob",
                Action::Cancel {
                    id: "run-1".into(),
                    owner: "bob".into(),
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
                r#"run count --id run-2 --input-json '{"name":"Élan"}'"#,
                Action::Run {
                    worker: "count".into(),
                    workflow: None,
                    id: "run-2".into(),
                    input: RunInput::Json(r#"{"name":"Élan"}"#.into()),
                    owner: "alice".into(),
                },
            ),
            (
                r#"run count --id run-2 --workflow count --input-json '{"name":"Élan"}'"#,
                Action::Run {
                    worker: "count".into(),
                    workflow: Some("count".into()),
                    id: "run-2".into(),
                    input: RunInput::Json(r#"{"name":"Élan"}"#.into()),
                    owner: "alice".into(),
                },
            ),
            (
                r#"run greet/bob --id run-1 --input-json '{"name":"Alice Smith","wait_seconds":5}'"#,
                Action::Run {
                    worker: "greet".into(),
                    workflow: None,
                    id: "run-1".into(),
                    input: RunInput::Json(r#"{"name":"Alice Smith","wait_seconds":5}"#.into()),
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
            ctx.parse(r#"run count/bob --id run-1 --input-json '{"name":"Alice"}'"#),
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
            "run greet run-1",
            "run greet --id run-1 Alice 5",
            r#"run greet --input-json '{"name":"Alice","wait_seconds":"nope"}'"#,
            "run other --id run-1",
            "status run-1/eve",
            "status run-1/",
            "status /bob",
            "status temporal-sys-forbidden/bob",
            "run greet/",
            "run greet/unknown",
            "list /bob",
            "list --owner bob",
            "secrets set api_token leaked_value",
            "secrets set ../escape",
            "workers; exit",
        ] {
            assert!(matches!(ctx.parse(line), Err(Error::Clap(_))), "{line}");
        }
        assert!(matches!(
            ctx.parse("run greet --input-json 'unfinished"),
            Err(Error::Quote)
        ));
        assert!(
            matches!(ctx.parse("help"), Err(Error::Clap(err)) if err.kind() == ErrorKind::DisplayHelp)
        );
        assert!(matches!(ctx.parse("run --help"), Err(Error::Clap(err))
            if err.to_string().lines().any(|line| line == "Usage: boreal run [OPTIONS] [WORKER[/OWNER]]")));
    }

    #[test]
    fn test_shell_metacharacters_are_only_arguments() {
        let ctx = Context::mock();
        let parsed = ctx.parse(
            r#"run greet --id run-1 --input-json '{"name":"$(not-executed)","wait_seconds":5}'"#,
        );
        assert!(
            matches!(parsed, Ok(Some(Action::Run { input: RunInput::Json(raw), .. })) if raw.contains("$(not-executed)"))
        );
    }
}
