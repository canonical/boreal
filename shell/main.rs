mod client;
mod command;
mod form;
mod history;
mod local;
mod tui;
mod workflow_completion;

use std::env;
use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use boreal_common::catalog::{self, Catalog, DEFAULT_ETC_DIR};
use boreal_common::input;
use boreal_common::os;
use boreal_common::secret::{self, MAX_SECRET_BYTES, Store};
use boreal_common::wire::{BROKER_SOCKET, Request, Response, RunInput};
use boreal_shell::terminal::{self, Host, Invocation, Session, Submission, is_ssh_session};

use crate::client::Mode;
use crate::command::{Action, Context, Launch, Missing, SecretAction, Target};
use crate::local::LocalProfile;

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    c_ispeed: u32,
    c_ospeed: u32,
}

const ECHO: u32 = 0o000010;
const TCSANOW: i32 = 0;
const LOCAL_USAGE: &str = concat!(
    "usage: boreal-shell --local <profile-home> <catalog-dir> <broker-socket>",
    " [--secret-dir <absolute-path>] [command argv...]",
);

unsafe extern "C" {
    fn tcgetattr(fd: i32, termios_p: *mut Termios) -> i32;
    fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
}

struct EchoGuard {
    fd: i32,
    original: Termios,
}

impl EchoGuard {
    fn disable_stdin_echo() -> io::Result<Self> {
        let fd = io::stdin().as_raw_fd();
        let mut orig = std::mem::MaybeUninit::<Termios>::zeroed();
        if unsafe { tcgetattr(fd, orig.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let orig = unsafe { orig.assume_init() };
        let mut quiet = orig;
        quiet.c_lflag &= !ECHO;
        if unsafe { tcsetattr(fd, TCSANOW, &raw const quiet) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, original: orig })
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = unsafe { tcsetattr(self.fd, TCSANOW, &raw const self.original) };
    }
}

fn with_secret_store(
    local: Option<&LocalProfile>,
    f: impl FnOnce(&Store) -> Result<(), String>,
) -> Result<(), String> {
    if let Some(local) = local {
        return f(local.secret_store());
    }
    let store = if !Path::new(DEFAULT_ETC_DIR).exists()
        && !is_ssh_session()
        && let Ok(home) = env::var("BOREAL_TEST_HOME")
    {
        Store::from_home(home, os::current_euid())
    } else {
        Store::for_current_user().map_err(|e| e.detail())?
    };
    f(&store)
}

#[derive(Debug, PartialEq, Eq)]
enum ScriptCommand {
    SetSecret(String),
    Workflow(Vec<String>),
}

fn parse_ssh_command(raw_cmd: &str) -> Result<ScriptCommand, String> {
    let words = shlex::split(raw_cmd).ok_or_else(|| "invalid quoting in SSH command".to_owned())?;
    parse_script_command(words, false)
}

fn parse_script_command(words: Vec<String>, local: bool) -> Result<ScriptCommand, String> {
    match words.as_slice() {
        [cmd, sub, name] if cmd == "secrets" && sub == "set" => {
            secret::validate_name(name).map_err(|e| e.detail())?;
            Ok(ScriptCommand::SetSecret(name.clone()))
        }
        [cmd, ..]
            if matches!(cmd.as_str(), "run" | "list" | "status" | "result")
                || (local && cmd == "workers") =>
        {
            Ok(ScriptCommand::Workflow(words))
        }
        _ if local => Err(
            "unsupported local command; use run, list, status, result, workers, or 'secrets set <name>'"
                .into(),
        ),
        _ => Err(
            "unsupported SSH command; use run, list, status, result, or 'secrets set <name>'"
                .into(),
        ),
    }
}

fn run_script_command(
    command: ScriptCommand,
    local: Option<&LocalProfile>,
) -> Result<bool, Box<dyn Error>> {
    match command {
        ScriptCommand::SetSecret(name) => {
            run_stdin_secret_set(&name, local)?;
            Ok(true)
        }
        ScriptCommand::Workflow(words) => {
            let (ctx, mode) = load_runtime(local)?;
            match ctx.parse_words(&words) {
                Ok(Some(action)) => execute_action(action, &mode, local, false).map_err(Into::into),
                Ok(None) => Ok(false),
                Err(command::Error::Clap(err)) => {
                    // clap may echo an offending token, including an inline payload.
                    if err.use_stderr() {
                        if err.kind() == clap::error::ErrorKind::ValueValidation {
                            err.print()?;
                        } else {
                            eprintln!(
                                "boreal-shell: invalid {} command; use '{} --help'",
                                words[0], words[0]
                            );
                        }
                        Ok(false)
                    } else {
                        err.print()?;
                        Ok(true)
                    }
                }
                Err(command::Error::Quote) => Err("invalid quoting in SSH command".into()),
            }
        }
    }
}

fn run_stdin_secret_set(name: &str, local: Option<&LocalProfile>) -> Result<(), String> {
    if io::stdin().is_terminal() {
        return Err(
            "non-interactive 'secrets set <name>' requires piping secret bytes on stdin".into(),
        );
    }
    with_secret_store(local, |store| {
        let mut buf = Vec::new();
        io::stdin()
            .take((MAX_SECRET_BYTES + 1) as u64)
            .read_to_end(&mut buf)
            .map_err(|e| format!("read stdin: {e}"))?;
        let res = store.put(name, &buf);
        secret::zeroize(&mut buf);
        let written = res.map_err(|e| e.detail())?;
        println!("Stored secret '{name}' ({written} bytes).");
        Ok(())
    })
}

fn load_runtime(local: Option<&LocalProfile>) -> Result<(Arc<Context>, Mode), String> {
    if let Some(local) = local {
        return Ok(local.runtime());
    }
    let etc_path = Path::new(DEFAULT_ETC_DIR);
    if etc_path.exists() || is_ssh_session() {
        let user = os::user_by_uid(os::current_euid()).map_err(|e| e.detail())?;
        let catalog = Catalog::load_system().map_err(|e| e.detail())?;
        if !catalog.is_user(user.name()) {
            return Err(format!(
                "account '{}' is not authorized as a Boreal lab user",
                user.name()
            ));
        }
        let ctx = Arc::new(Context::new(catalog, user.name()));
        let mode = Mode::Live {
            socket: PathBuf::from(BROKER_SOCKET),
        };
        return Ok((ctx, mode));
    }

    // Local developer preview / non-SSH test harness only when /etc/boreal is absent.
    if let Ok(test_etc) = env::var("BOREAL_TEST_ETC_DIR") {
        let catalog = Catalog::load_from_dir(Path::new(&test_etc), None).map_err(|e| e.detail())?;
        let caller = match env::var("BOREAL_TEST_CALLER") {
            Ok(c) => c,
            Err(_) => os::user_by_uid(os::current_euid())
                .map_err(|e| e.detail())?
                .name()
                .to_owned(),
        };
        if !catalog.is_user(&caller) {
            return Err(format!("account '{caller}' is not in test catalog"));
        }
        let sock = env::var("BOREAL_TEST_BROKER_SOCKET")
            .map_or_else(|_| PathBuf::from(BROKER_SOCKET), PathBuf::from);
        return Ok((
            Arc::new(Context::new(catalog, caller)),
            Mode::Live { socket: sock },
        ));
    }

    let catalog = Catalog::mock();
    let caller = String::from("alice");
    Ok((
        Arc::new(Context::new(catalog.clone(), caller.clone())),
        Mode::mock(catalog, caller),
    ))
}

fn confirm_prompt(question: &str) -> io::Result<bool> {
    print!("{question} [y/N]: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    let n = io::stdin().lock().read_line(&mut answer)?;
    if n == 0 {
        println!();
        return Ok(false);
    }
    let trimmed = answer.trim();
    Ok(trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes"))
}

fn handle_secret_action(action: SecretAction, store: &Store) -> Result<(), String> {
    match action {
        SecretAction::List => {
            let entries = store.list().map_err(|e| e.detail())?;
            if entries.is_empty() {
                println!("no secrets stored.");
            } else {
                for entry in entries {
                    println!("{} ({} bytes, mode 0600)", entry.name(), entry.size());
                }
            }
            Ok(())
        }
        SecretAction::Set { name } => {
            if store.exists(&name).map_err(|e| e.detail())? {
                let confirmed = confirm_prompt(&format!("Overwrite existing secret '{name}'?"))
                    .map_err(|e| e.to_string())?;
                if !confirmed {
                    println!("aborted.");
                    return Ok(());
                }
            }
            print!("Enter secret value for '{name}' (input hidden): ");
            io::stdout().flush().map_err(|e| e.to_string())?;
            let guard = EchoGuard::disable_stdin_echo().map_err(|e| e.to_string())?;
            let mut raw = Vec::new();
            let read_res = io::stdin().lock().read_until(b'\n', &mut raw);
            drop(guard);
            println!();
            read_res.map_err(|e| e.to_string())?;
            while matches!(raw.last(), Some(b'\n' | b'\r')) {
                let idx = raw.len() - 1;
                unsafe { std::ptr::write_volatile(&mut raw[idx], 0) };
                raw.pop();
            }
            let res = store.put(&name, &raw);
            secret::zeroize(&mut raw);
            let count = res.map_err(|e| e.detail())?;
            println!("Stored secret '{name}' ({count} bytes).");
            Ok(())
        }
        SecretAction::Remove { name } => {
            if !store.exists(&name).map_err(|e| e.detail())? {
                return Err(format!("secret '{name}' does not exist"));
            }
            let confirmed =
                confirm_prompt(&format!("Remove secret '{name}'?")).map_err(|e| e.to_string())?;
            if !confirmed {
                println!("aborted.");
                return Ok(());
            }
            store.remove(&name).map_err(|e| e.detail())?;
            println!("Removed secret '{name}'.");
            Ok(())
        }
    }
}

fn execute_action(
    action: Action,
    mode: &Mode,
    local: Option<&LocalProfile>,
    interactive: bool,
) -> Result<bool, String> {
    match action {
        Action::Exit => Ok(true),
        Action::Prompt(_) => Err("unresolved interactive prompt".into()),
        Action::Secrets(sec) => {
            with_secret_store(local, |store| handle_secret_action(sec, store)).map(|()| true)
        }
        Action::Workers => {
            let resp = mode.send(&Request::Workers)?;
            Ok(client::print_response(resp, mode.is_mock()))
        }
        Action::Permissions { user } => {
            let resp = mode.send(&Request::Permissions { user: Some(user) })?;
            Ok(client::print_response(resp, mode.is_mock()))
        }
        Action::Run {
            worker,
            workflow,
            id,
            input,
            owner,
        } => {
            let input = match input {
                RunInput::Json(raw) if raw == "-" => {
                    if interactive || io::stdin().is_terminal() {
                        let method = if local.is_some() {
                            "a non-interactive local command"
                        } else {
                            "non-interactive SSH without a PTY"
                        };
                        return Err(format!("--input-json - reads piped stdin; use {method}"));
                    }
                    let mut raw = String::new();
                    io::stdin()
                        .take((input::MAX_BYTES + 1) as u64)
                        .read_to_string(&mut raw)
                        .map_err(|e| format!("read JSON from stdin: {e}"))?;
                    input::parse(&raw)?;
                    RunInput::Json(raw)
                }
                other => other,
            };
            let resp = mode.send(&Request::Run {
                owner,
                worker,
                workflow,
                id,
                input,
            })?;
            Ok(client::print_response(resp, mode.is_mock()))
        }
        Action::List { owner } => {
            let resp = mode.send(&Request::List { owner })?;
            Ok(client::print_response(resp, mode.is_mock()))
        }
        Action::Status { id, owner } => {
            let resp = mode.send(&Request::Status { owner, id })?;
            Ok(client::print_response(resp, mode.is_mock()))
        }
        Action::Result { id, owner } => {
            let resp = mode.send(&Request::Result { owner, id })?;
            Ok(client::print_response(resp, mode.is_mock()))
        }
        Action::View { id, owner } => {
            let resp = mode.send(&Request::View {
                owner: owner.clone(),
                id: id.clone(),
            })?;
            match resp {
                Response::View(initial) => {
                    tui::run(initial, &owner, &id, mode)?;
                    Ok(true)
                }
                other => Ok(client::print_response(other, mode.is_mock())),
            }
        }
        Action::Cancel { id, owner } => {
            let prep = mode.send(&Request::PrepareCancel {
                owner: owner.clone(),
                id: id.clone(),
            })?;
            let detail = match prep {
                Response::CancelPrepared(d) => d,
                other => {
                    return Ok(client::print_response(other, mode.is_mock()));
                }
            };
            let prompt = format!(
                "Cancel {} '{}' (run {}, owner {})?",
                detail.workflow_type, detail.workflow_id, detail.run_id, detail.owner
            );
            let confirmed = confirm_prompt(&prompt).map_err(|e| e.to_string())?;
            if !confirmed {
                println!("cancellation aborted.");
                return Ok(true);
            }
            let resp = mode.send(&Request::Cancel {
                owner,
                id,
                run_id: detail.run_id,
            })?;
            Ok(client::print_response(resp, mode.is_mock()))
        }
    }
}

fn choose_account(ctx: &Context, writable: bool) -> Result<Option<String>, String> {
    let accounts = if writable {
        ctx.catalog().writable_owners(ctx.caller())
    } else {
        ctx.catalog().readable_owners(ctx.caller())
    };
    form::choose(
        "--owner",
        &accounts.into_iter().map(str::to_owned).collect::<Vec<_>>(),
    )
}

fn complete_launch(mut launch: Launch, ctx: &Context) -> Result<Option<Action>, String> {
    let owner = match launch.owner.as_ref() {
        Some(owner) => owner.clone(),
        None => match choose_account(ctx, true)? {
            Some(owner) => owner,
            None => return Ok(None),
        },
    };
    let worker = match launch.worker.as_ref() {
        Some(worker) => worker.clone(),
        None => {
            let policy = ctx.catalog().require_user(&owner).map_err(|e| e.detail())?;
            let names: Vec<_> = policy
                .workers()
                .keys()
                .filter(|worker| ctx.catalog().can_write(ctx.caller(), &owner, worker))
                .cloned()
                .collect();
            let Some(worker) = form::choose("Worker", &names)? else {
                return Ok(None);
            };
            worker
        }
    };
    let package = ctx.run_package(&owner, &worker)?;
    if launch.workflow.is_none() && package.only_workflow().is_none() {
        let names: Vec<_> = package
            .workflows()
            .map(|(alias, _)| alias.to_owned())
            .collect();
        let Some(alias) = form::choose("--workflow", &names)? else {
            return Ok(None);
        };
        launch.workflow = Some(alias);
    }
    let workflow = package
        .select(launch.workflow.as_deref())
        .map_err(|e| e.detail())?;
    launch.owner = Some(owner);
    launch.worker = Some(worker);
    if launch.input_json.is_none() {
        let initial = input::Value::Object(Default::default());
        let default_id = launch
            .id
            .as_ref()
            .map_or_else(command::new_id, Clone::clone);
        let Some((id, raw)) = form::run(launch.id, default_id, workflow.input_schema(), initial)?
        else {
            return Ok(None);
        };
        launch.id = Some(id);
        launch.input_json = Some(raw);
    }
    ctx.prepare_run(launch, false).map(Some)
}

fn choose_workflow_id(
    ctx: &Context,
    mode: &Mode,
    owner: &str,
    kind: Target,
) -> Result<Option<String>, String> {
    if ctx.catalog().has_any_read(ctx.caller(), owner)
        && let Response::List(entries) = mode.send(&Request::List {
            owner: owner.to_owned(),
        })?
    {
        let mut ids: Vec<_> = entries
            .into_iter()
            .filter(|entry| {
                kind != Target::Cancel
                    || ctx.catalog().can_write(ctx.caller(), owner, &entry.worker)
            })
            .map(|entry| entry.workflow_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if !ids.is_empty() {
            return form::choose("Workflow ID", &ids);
        }
    }
    form::text("Workflow ID")
}

fn complete_action(action: Action, ctx: &Context, mode: &Mode) -> Result<Option<Action>, String> {
    match action {
        Action::Prompt(Missing::Run(launch)) => complete_launch(launch, ctx),
        Action::Prompt(Missing::Target { kind, owner, id }) => {
            let owner = match owner {
                Some(owner) => owner,
                None => match choose_account(ctx, kind == Target::Cancel)? {
                    Some(owner) => owner,
                    None => return Ok(None),
                },
            };
            let id = if kind == Target::List || id.is_some() {
                id
            } else {
                let Some(id) = choose_workflow_id(ctx, mode, &owner, kind)? else {
                    return Ok(None);
                };
                catalog::validate_workflow_id(&id).map_err(|e| e.detail())?;
                Some(id)
            };
            Ok(kind.action(owner, id))
        }
        Action::Prompt(Missing::Permissions) => {
            let names: Vec<_> = ctx
                .catalog()
                .visible_permission_users(ctx.caller())
                .into_iter()
                .map(str::to_owned)
                .collect();
            Ok(form::choose("Account", &names)?.map(|user| Action::Permissions { user }))
        }
        Action::Prompt(Missing::Secret { remove }) => form::text("Secret name")?
            .map(|name| command::named_secret(name, remove))
            .transpose(),
        other => Ok(Some(other)),
    }
}

fn interactive_home(uid: u32) -> Result<PathBuf, String> {
    if !Path::new(DEFAULT_ETC_DIR).exists()
        && !is_ssh_session()
        && let Ok(test_home) = env::var("BOREAL_TEST_HOME")
    {
        Ok(PathBuf::from(test_home))
    } else {
        os::user_by_uid(uid)
            .map(|user| user.home().to_owned())
            .map_err(|err| err.detail())
    }
}

fn load_history(home: Result<&Path, &str>, uid: u32) -> Box<dyn reedline::History> {
    match home
        .map_err(str::to_owned)
        .and_then(|home| history::open(home, uid).map_err(|err| err.to_string()))
    {
        Ok(history) => history,
        Err(e) => {
            eprintln!(
                "boreal-shell: warning: cannot open ~/boreal.history ({e}); using session history"
            );
            history::memory()
        }
    }
}

fn resolve_prompt_host(is_mock: bool) -> Host {
    if is_mock {
        Host::Mock
    } else if !Path::new(DEFAULT_ETC_DIR).exists()
        && !is_ssh_session()
        && let Ok(test_host) = env::var("BOREAL_TEST_HOSTNAME")
    {
        Host::Live(os::sanitize_hostname(test_host))
    } else {
        Host::live_system()
    }
}

fn run_interactive(local: Option<&LocalProfile>) -> Result<(), Box<dyn Error>> {
    terminal::require_tty("boreal-shell")?;
    let (ctx, mode) = load_runtime(local)?;
    let profile_home = local.map(LocalProfile::home);

    if local.is_some() {
        println!(
            "Boreal local shell ({}). Type help for commands.",
            ctx.caller()
        );
    } else if mode.is_mock() {
        println!("Boreal shell preview: standalone mock-only mode (/etc/boreal not present).");
        println!("Type help for commands.");
    } else {
        println!(
            "Boreal lab shell ({}). Type help for commands.",
            ctx.caller()
        );
    }

    let uid = os::current_euid();
    let account_home;
    let home = match profile_home {
        Some(home) => Ok(home),
        None => {
            account_home = interactive_home(uid);
            account_home.as_deref().map_err(String::as_str)
        }
    };
    Session {
        program: "boreal-shell",
        caller: ctx.caller(),
        host: if local.is_some() {
            Host::Local
        } else {
            resolve_prompt_host(mode.is_mock())
        },
        home,
        history: Some(load_history(home, uid)),
    }
    .run(
        Arc::clone(&ctx),
        || mode.prompt_summary(),
        |line| match ctx.parse_interactive(line) {
            Ok(Some(Action::Exit)) => Ok(Submission::Exit),
            Ok(Some(action)) => complete_action(action, &ctx, &mode)
                .and_then(|action| {
                    action.map_or(Ok(true), |action| {
                        execute_action(action, &mode, local, true)
                    })
                })
                .map(Submission::Continue)
                .map_err(terminal::Error::Command),
            Ok(None) => Ok(Submission::Continue(true)),
            Err(command::Error::Quote) => Err(terminal::Error::Quote),
            Err(command::Error::Clap(err)) => Err(terminal::Error::Clap(err)),
        },
    )
}

fn run() -> Result<bool, Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    // Check before Invocation's SSH_ORIGINAL_COMMAND handling so SSH cannot
    // select local mode or cause a local invocation to fall through to SSH.
    if args.next().is_some_and(|arg| arg == "--local") {
        if is_ssh_session() {
            return Err("--local is unavailable in SSH sessions".into());
        }
        let args: Vec<_> = args.collect();
        let [home, catalog, socket, command @ ..] = args.as_slice() else {
            return Err(LOCAL_USAGE.into());
        };
        let (secret_dir, command) = match command {
            [flag, path, command @ ..] if flag == "--secret-dir" => {
                (Some(path.as_os_str()), command)
            }
            [flag] if flag == "--secret-dir" => return Err(LOCAL_USAGE.into()),
            _ => (None, command),
        };
        let profile = LocalProfile::open(home, catalog, socket, secret_dir)?;
        if command.is_empty() {
            return run_interactive(Some(&profile)).map(|()| true);
        }
        let words = command
            .iter()
            .map(|arg| arg.to_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or("local command arguments must be valid UTF-8")?;
        return run_script_command(parse_script_command(words, true)?, Some(&profile));
    }
    match Invocation::from_env()? {
        Invocation::Interactive => run_interactive(None).map(|()| true),
        Invocation::Command(command) => run_script_command(parse_ssh_command(&command)?, None),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("boreal-shell: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_command_filter_only_permits_allowlisted_commands() {
        assert_eq!(
            parse_ssh_command("secrets set api_token").unwrap(),
            ScriptCommand::SetSecret("api_token".into())
        );
        assert!(matches!(
            parse_ssh_command("run demo --id demo-0 --input-json -"),
            Ok(ScriptCommand::Workflow(_))
        ));
        for rejected in [
            "",
            "/bin/sh",
            "id",
            "sftp-server",
            "scp -t /tmp/a",
            "--local /profile /catalog /socket list",
            "workers",
            "secrets list",
            "secrets set api_token inline_secret",
            "secrets set ../bad",
        ] {
            assert!(parse_ssh_command(rejected).is_err(), "{rejected}");
        }
    }
}
