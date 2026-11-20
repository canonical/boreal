use std::io::{self, IsTerminal};
use std::path::Path;
use std::sync::Arc;
use std::{env, error};

use boreal_common::{os, wire::PromptSummary};
use crossterm::cursor::SetCursorStyle;
use nu_ansi_term::Color;
use reedline::{
    CursorConfig, DefaultHinter, EditMode, Emacs, KeyCode, KeyModifiers, Keybindings, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, Vi, default_emacs_keybindings,
    default_vi_insert_keybindings, default_vi_normal_keybindings,
};

use crate::completion::{Completer, Grammar};
use crate::highlighter::Highlighter;
use crate::prompt::{self, Prompt};
use crate::{helix, menu, rc};

pub use crate::prompt::Host;

const DEPLOYMENT_FILE: &str = "/etc/boreal-deployment";
const MAX_LABEL_BYTES: usize = 128;
const UNKNOWN_DEPLOYMENT: &str = "unknown";

pub enum Invocation {
    Interactive,
    Command(String),
}

impl Invocation {
    pub fn from_env() -> Result<Self, &'static str> {
        if let Some(raw) = env::var_os("SSH_ORIGINAL_COMMAND") {
            return Ok(Self::Command(
                raw.into_string()
                    .map_err(|_| "SSH command must be valid UTF-8")?,
            ));
        }

        let args: Vec<_> = env::args_os().skip(1).collect();
        match args.as_slice() {
            [] => Ok(Self::Interactive),
            [flag, raw] if flag == "-c" => Ok(Self::Command(
                raw.to_str()
                    .ok_or("SSH command must be valid UTF-8")?
                    .to_owned(),
            )),
            _ => Err("unsupported invocation; use an interactive session or -c COMMAND"),
        }
    }
}

pub fn is_ssh_session() -> bool {
    [
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "SSH_TTY",
        "SSH_ORIGINAL_COMMAND",
    ]
    .into_iter()
    .any(|name| env::var_os(name).is_some())
}

pub fn require_tty(program: &str) -> Result<(), String> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        Ok(())
    } else {
        Err(format!("{program} requires an interactive terminal"))
    }
}

pub enum Submission {
    Continue(bool),
    Exit,
}

pub enum Error {
    Quote,
    Clap(clap::Error),
    Command(String),
}

pub struct Session<'a> {
    pub program: &'a str,
    pub caller: &'a str,
    pub host: Host,
    pub home: Result<&'a Path, &'a str>,
    /// `None` uses Reedline's session-only history.
    pub history: Option<Box<dyn reedline::History>>,
}

impl Session<'_> {
    pub fn run<G: Grammar + Send + Sync + 'static>(
        self,
        grammar: Arc<G>,
        mut summary: impl FnMut() -> Option<PromptSummary>,
        mut submit: impl FnMut(&str) -> Result<Submission, Error>,
    ) -> Result<(), Box<dyn error::Error>> {
        let (edit_mode, helix_state) = build_edit_mode(load_edit_mode(self.program, self.home));
        let mut editor = Reedline::create()
            .with_completer(Box::new(Completer::new(Arc::clone(&grammar))))
            .with_quick_completions(true)
            .with_highlighter(Box::new(Highlighter::new(grammar)))
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
        let sync_history = self.history.is_some();
        if let Some(history) = self.history {
            editor = editor.with_history(history);
        }

        let deployment = match self.host {
            Host::Mock => UNKNOWN_DEPLOYMENT.to_owned(),
            Host::Local => "local profile".to_owned(),
            Host::Live(_) => deployment_label(),
        };
        let mut prompt = Prompt::new(self.caller, self.host, &deployment, helix_state);
        let _title = prompt::TitleGuard::enter(prompt.window_title());
        let mut last_ok = true;
        let mut warned_history = false;

        loop {
            println!();
            prompt.refresh(summary(), last_ok);
            match editor.read_line(&prompt)? {
                Signal::Success(line) => {
                    if sync_history
                        && let Err(err) = editor.sync_history()
                        && !warned_history
                    {
                        eprintln!(
                            "{}: warning: could not sync ~/boreal.history ({err})",
                            self.program
                        );
                        warned_history = true;
                    }
                    last_ok = match submit(&line) {
                        Ok(Submission::Exit) => break,
                        Ok(Submission::Continue(ok)) => ok,
                        Err(Error::Quote) => {
                            eprintln!("error: unfinished quote or escape");
                            false
                        }
                        Err(Error::Clap(err)) => {
                            err.print()?;
                            !err.use_stderr()
                        }
                        Err(Error::Command(err)) => {
                            eprintln!("error: {err}");
                            false
                        }
                    };
                }
                Signal::CtrlD | Signal::ExternalBreak(_) => break,
                _ => continue,
            }
        }
        Ok(())
    }
}

fn deployment_label() -> String {
    os::read_owned_file(
        Path::new(DEPLOYMENT_FILE),
        Some(0),
        0o222,
        MAX_LABEL_BYTES + 1,
    )
    .ok()
    .and_then(|bytes| parse_label(&bytes).map(str::to_owned))
    .unwrap_or_else(|| UNKNOWN_DEPLOYMENT.to_owned())
}

fn parse_label(bytes: &[u8]) -> Option<&str> {
    let label = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if label.is_empty()
        || label.len() > MAX_LABEL_BYTES
        || !label
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    {
        return None;
    }
    std::str::from_utf8(label).ok()
}

fn load_edit_mode(program: &str, home: Result<&Path, &str>) -> rc::Mode {
    let mode = home.map_err(str::to_owned).and_then(|home| {
        rc::load_from_home(home, os::current_euid())
            .map(|cfg| cfg.edit_mode())
            .map_err(|err| format!("invalid ~/.borealrc ({})", err.detail()))
    });
    match mode {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("{program}: warning: {err}; falling back to emacs mode");
            rc::Mode::default()
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
                Box::new(helix::Helix::with_state(insert_keys, Arc::clone(&state))),
                Some(state),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deployment_label_accepts_only_bounded_display_text() {
        for label in ["d9c4975", "d9c4975+", "v1.2.3-rc.1+build.2", "unknown"] {
            assert_eq!(parse_label(format!("{label}\n").as_bytes()), Some(label));
        }
        for value in ["", "\n", "v1.2.3\nnext", "v1.2.3 ", "v1.2.3\x1b"] {
            assert_eq!(parse_label(value.as_bytes()), None);
        }
        assert_eq!(parse_label(&[b'a'; MAX_LABEL_BYTES + 1]), None);
    }
}
