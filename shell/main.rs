mod client;
mod command;
mod completion;
mod helix;
mod highlighter;
mod menu;
mod prompt;
mod rc;

use std::env;
use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use boreal_common::catalog::{Catalog, DEFAULT_ETC_DIR};
use boreal_common::os;
use boreal_common::secret::{self, MAX_SECRET_BYTES, Store};
use boreal_common::wire::{BROKER_SOCKET, Request, Response};
use crossterm::cursor::SetCursorStyle;
use nu_ansi_term::Color;
use reedline::{
    CursorConfig, DefaultHinter, EditMode, Emacs, KeyCode, KeyModifiers, Keybindings, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, Vi, default_emacs_keybindings,
    default_vi_insert_keybindings, default_vi_normal_keybindings,
};

use crate::client::Mode;
use crate::command::{Action, Context, SecretAction};
use crate::helix::Helix;
use crate::prompt::Prompt;
use crate::rc::Config;

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

fn is_ssh_session() -> bool {
    env::var_os("SSH_CONNECTION").is_some()
        || env::var_os("SSH_CLIENT").is_some()
        || env::var_os("SSH_TTY").is_some()
        || env::var_os("SSH_ORIGINAL_COMMAND").is_some()
}

fn secret_store() -> Result<Store, String> {
    if !Path::new(DEFAULT_ETC_DIR).exists()
        && !is_ssh_session()
        && let Ok(home) = env::var("BOREAL_TEST_HOME")
    {
        return Ok(Store::from_home(home, os::current_euid()));
    }
    Store::for_current_user().map_err(|e| e.detail())
}

fn parse_ssh_secret_command(raw_cmd: &str) -> Result<String, String> {
    let words = shlex::split(raw_cmd).ok_or_else(|| "invalid quoting in SSH command".to_owned())?;
    match words.as_slice() {
        [cmd, sub, name] if cmd == "secrets" && sub == "set" => {
            secret::validate_name(name).map_err(|e| e.detail())?;
            Ok(name.clone())
        }
        _ => Err(
            "unsupported SSH command or subsystem; connect interactively or pipe stdin to 'secrets set <name>'"
                .into(),
        ),
    }
}

fn run_stdin_secret_set(name: &str) -> Result<(), String> {
    if io::stdin().is_terminal() {
        return Err(
            "non-interactive 'secrets set <name>' requires piping secret bytes on stdin".into(),
        );
    }
    let store = secret_store()?;
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
}

fn load_runtime() -> Result<(Arc<Context>, Mode), String> {
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

fn handle_secret_action(action: SecretAction) -> Result<(), String> {
    let store = secret_store()?;
    match action {
        SecretAction::List => {
            let entries = store.list().map_err(|e| e.detail())?;
            if entries.is_empty() {
                println!("no secrets stored in ~/.config/boreal/secrets.");
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

fn execute_action(action: Action, mode: &Mode) -> Result<bool, String> {
    match action {
        Action::Exit => Ok(true),
        Action::Secrets(sec) => handle_secret_action(sec).map(|()| true),
        Action::Workers => {
            let resp = mode.send(&Request::Workers)?;
            Ok(client::print_response(resp, mode.is_mock()))
        }
        Action::Permissions { user } => {
            let resp = mode.send(&Request::Permissions { user: Some(user) })?;
            Ok(client::print_response(resp, mode.is_mock()))
        }
        Action::Start {
            worker,
            id,
            args,
            owner,
        } => {
            let resp = mode.send(&Request::Start {
                owner,
                worker,
                id,
                args,
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

fn load_rc_config() -> Config {
    let uid = os::current_euid();
    let home = if !Path::new(DEFAULT_ETC_DIR).exists()
        && !is_ssh_session()
        && let Ok(test_home) = env::var("BOREAL_TEST_HOME")
    {
        PathBuf::from(test_home)
    } else {
        match os::user_by_uid(uid) {
            Ok(user) => PathBuf::from(user.home()),
            Err(err) => {
                eprintln!(
                    "boreal-shell: warning: {}; falling back to emacs mode",
                    err.detail()
                );
                return Config::default();
            }
        }
    };
    match rc::load_from_home(&home, uid) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!(
                "boreal-shell: warning: invalid ~/.borealrc ({}); falling back to emacs mode",
                err.detail()
            );
            Config::default()
        }
    }
}

fn bind_completion_keys(keys: &mut Keybindings) {
    keys.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keys.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    keys.add_binding(
        KeyModifiers::NONE,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
}

fn build_edit_mode(mode: rc::Mode) -> (Box<dyn EditMode>, Option<Arc<helix::State>>) {
    match mode {
        rc::Mode::Emacs => {
            let mut keys = default_emacs_keybindings();
            bind_completion_keys(&mut keys);
            (Box::new(Emacs::new(keys)), None)
        }
        rc::Mode::Vi => {
            let mut insert_keys = default_vi_insert_keybindings();
            bind_completion_keys(&mut insert_keys);
            let normal_keys = default_vi_normal_keybindings();
            (Box::new(Vi::new(insert_keys, normal_keys)), None)
        }
        rc::Mode::Helix => {
            let mut insert_keys = default_vi_insert_keybindings();
            bind_completion_keys(&mut insert_keys);
            let state = Arc::new(helix::State::new());
            (
                Box::new(Helix::with_state(insert_keys, Arc::clone(&state))),
                Some(state),
            )
        }
    }
}

fn resolve_prompt_host(is_mock: bool) -> prompt::Host {
    if is_mock {
        prompt::Host::Mock
    } else if !Path::new(DEFAULT_ETC_DIR).exists()
        && !is_ssh_session()
        && let Ok(test_host) = env::var("BOREAL_TEST_HOSTNAME")
    {
        prompt::Host::Live(os::sanitize_hostname(test_host))
    } else {
        prompt::Host::live_system()
    }
}

fn run_interactive() -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("boreal-shell requires an interactive terminal".into());
    }
    let (ctx, mode) = load_runtime()?;
    let rc_cfg = load_rc_config();

    if mode.is_mock() {
        println!("Boreal shell preview: standalone mock-only mode (/etc/boreal not present).");
        println!("Type help for commands; history is kept in memory for this session.");
    } else {
        println!(
            "Boreal lab shell ({}). Type help for commands; history is kept in memory.",
            ctx.caller()
        );
    }

    let (edit_mode, helix_state) = build_edit_mode(rc_cfg.edit_mode());
    let mut editor = Reedline::create()
        .with_completer(Box::new(completion::Completer::new(Arc::clone(&ctx))))
        .with_quick_completions(true)
        .with_highlighter(Box::new(highlighter::Highlighter::new(Arc::clone(&ctx))))
        .with_hinter(Box::new(
            DefaultHinter::default().with_style(Color::DarkGray.normal()),
        ))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu::Menu::new())))
        .with_cursor_config(CursorConfig {
            emacs: Some(SetCursorStyle::SteadyBar),
            vi_insert: Some(SetCursorStyle::SteadyBar),
            vi_normal: Some(SetCursorStyle::SteadyBlock),
        })
        .with_edit_mode(edit_mode);

    let mut prompt = Prompt::new(
        ctx.caller(),
        resolve_prompt_host(mode.is_mock()),
        helix_state,
    );
    let mut last_ok = true;
    let mut last_duration = None;

    loop {
        println!();
        prompt.refresh(mode.prompt_summary(), last_ok, last_duration);
        match editor.read_line(&prompt)? {
            Signal::Success(line) => match ctx.parse(&line) {
                Ok(Some(Action::Exit)) => break,
                Ok(Some(action)) => {
                    let started = Instant::now();
                    match execute_action(action, &mode) {
                        Ok(ok) => {
                            last_ok = ok;
                            last_duration = Some(started.elapsed());
                        }
                        Err(err) => {
                            eprintln!("error: {err}");
                            last_ok = false;
                            last_duration = Some(started.elapsed());
                        }
                    }
                }
                Ok(None) => {
                    last_ok = true;
                    last_duration = None;
                }
                Err(command::Error::Quote) => {
                    eprintln!("error: unfinished quote or escape");
                    last_ok = false;
                    last_duration = None;
                }
                Err(command::Error::Clap(error)) => {
                    last_ok = !error.use_stderr();
                    last_duration = None;
                    error.print()?;
                }
            },
            Signal::CtrlC => {
                last_duration = None;
                continue;
            }
            Signal::CtrlD | Signal::ExternalBreak(_) => break,
            _ => continue,
        }
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn Error>> {
    if let Some(orig_os) = env::var_os("SSH_ORIGINAL_COMMAND") {
        let orig = orig_os
            .into_string()
            .map_err(|_| "SSH command must be valid UTF-8")?;
        let name = parse_ssh_secret_command(&orig)?;
        run_stdin_secret_set(&name)?;
        return Ok(());
    }

    let args: Vec<_> = env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => run_interactive(),
        [flag, cmd] if flag == "-c" => {
            let cmd_str = cmd
                .to_str()
                .ok_or("SSH command argument must be valid UTF-8")?;
            let name = parse_ssh_secret_command(cmd_str)?;
            run_stdin_secret_set(&name)?;
            Ok(())
        }
        _ => Err(
            "unsupported command-line invocation; connect interactively over SSH or pipe stdin to 'secrets set <name>'"
                .into(),
        ),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
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
    fn test_ssh_command_filter_only_permits_stdin_secret_set() {
        assert_eq!(
            parse_ssh_secret_command("secrets set api_token").unwrap(),
            "api_token"
        );
        for rejected in [
            "",
            "/bin/sh",
            "id",
            "sftp-server",
            "scp -t /tmp/a",
            "workers",
            "secrets list",
            "secrets set api_token inline_secret",
            "secrets set ../bad",
        ] {
            assert!(parse_ssh_secret_command(rejected).is_err(), "{rejected}");
        }
    }
}
