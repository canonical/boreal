mod policy;

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::process::{Command, ExitCode};
use std::sync::Arc;

use clap::builder::{PossibleValue, PossibleValuesParser};
use clap::error::ErrorKind;
use clap::{Arg, Command as ClapCommand};
use crossterm::cursor::SetCursorStyle;
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, Completer as ReedlineCompleter, CursorConfig, DefaultHinter, DefaultPrompt,
    DefaultPromptSegment, Emacs, KeyCode, KeyModifiers, MenuBuilder, OutputMode, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, Span, StyledText, Suggestion, default_emacs_keybindings,
};

use crate::policy::AdminPolicy;

const SUDO_BIN: &str = "/run/wrappers/bin/sudo";

#[derive(Clone, Debug, PartialEq, Eq)]
enum AdminAction {
    Services,
    Service { op: String, unit: String },
    UpdateSchedule { rev: String },
    UpdatePrebuild,
    UpdateApply,
    UpdateRecover,
    UpdateStatus,
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
                ClapCommand::new("update")
                    .about("Schedule, prebuild, apply, or inspect NixOS lab updates")
                    .subcommand_required(true)
                    .subcommand(
                        ClapCommand::new("schedule")
                            .about("Schedule a 40-digit downstream Git commit")
                            .arg(Arg::new("rev").required(true)),
                    )
                    .subcommand(ClapCommand::new("prebuild").about("Prebuild the scheduled commit"))
                    .subcommand(
                        ClapCommand::new("apply")
                            .about("Close gate, drain work, and switch to prebuilt closure"),
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
        // Reject shell chaining/redirection tokens explicitly even if unquoted.
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
            Some(("update", sub)) => match sub.subcommand() {
                Some(("schedule", s)) => {
                    let rev = s.get_one::<String>("rev").unwrap().clone();
                    if rev.len() != 40 || !rev.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err(ParseErr::Clap(cmd.error(
                            ErrorKind::ValueValidation,
                            "revision must be a full 40-digit hexadecimal Git commit hash",
                        )));
                    }
                    AdminAction::UpdateSchedule {
                        rev: rev.to_ascii_lowercase(),
                    }
                }
                Some(("prebuild", _)) => AdminAction::UpdatePrebuild,
                Some(("apply", _)) => AdminAction::UpdateApply,
                Some(("recover", _)) => AdminAction::UpdateRecover,
                Some(("status", _)) => AdminAction::UpdateStatus,
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
            AdminAction::UpdateSchedule { rev } => {
                Some(vec!["update".into(), "schedule".into(), rev.clone()])
            }
            AdminAction::UpdatePrebuild => Some(vec!["update".into(), "prebuild".into()]),
            AdminAction::UpdateApply => Some(vec!["update".into(), "apply".into()]),
            AdminAction::UpdateRecover => Some(vec!["update".into(), "recover".into()]),
            AdminAction::UpdateStatus => Some(vec!["update".into(), "status".into()]),
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

struct Completer {
    grammar: Arc<AdminGrammar>,
}

impl ReedlineCompleter for Completer {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        if pos > line.len() || !line.is_char_boundary(pos) {
            return Vec::new();
        }
        let before = &line[..pos];
        let start = before.rfind([' ', '\t']).map_or(0, |idx| idx + 1);
        let prefix = &before[start..];
        let Some(prev) = shlex::split(&before[..start]) else {
            return Vec::new();
        };
        let mut cmd = self.grammar.cli();
        let mut args = vec![OsString::from("boreal-admin")];
        args.extend(prev.into_iter().map(OsString::from));
        let index = args.len();
        args.push(OsString::from(prefix));

        let Ok(candidates) = clap_complete::engine::complete(&mut cmd, args, index, None) else {
            return Vec::new();
        };
        candidates
            .into_iter()
            .filter_map(|c| {
                let val = c.get_value().to_str()?.to_owned();
                if !val.starts_with(prefix) {
                    return None;
                }
                Some(Suggestion {
                    value: val,
                    description: c.get_help().map(ToString::to_string),
                    span: Span::new(start, pos),
                    append_whitespace: pos == line.len(),
                    ..Suggestion::default()
                })
            })
            .collect()
    }
}

struct SimpleHighlighter {
    commands: Vec<String>,
}

impl SimpleHighlighter {
    fn new(grammar: &AdminGrammar) -> Self {
        let mut cli = grammar.cli();
        cli.build();
        let commands = cli
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        Self { commands }
    }
}

impl reedline::Highlighter for SimpleHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut out = StyledText::new();
        let trimmed = line.trim_start();
        let lead = line.len() - trimmed.len();
        if lead > 0 {
            out.push((Style::new(), line[..lead].to_owned()));
        }
        let end = trimmed.find([' ', '\t']).unwrap_or(trimmed.len());
        let first = &trimmed[..end];
        if !first.is_empty() {
            let style = if self.commands.iter().any(|c| c == first) {
                Style::new().fg(Color::Green)
            } else if self.commands.iter().any(|c| c.starts_with(first)) {
                Style::new().underline()
            } else {
                Style::new().fg(Color::Red)
            };
            out.push((style, first.to_owned()));
            if end < trimmed.len() {
                out.push((Style::new(), trimmed[end..].to_owned()));
            }
        }
        out
    }
}

fn load_policy() -> Result<AdminPolicy, String> {
    if !std::path::Path::new(policy::DEFAULT_ADMIN_POLICY).exists()
        && env::var_os("SSH_CONNECTION").is_none()
        && let Ok(test_path) = env::var("BOREAL_TEST_ADMIN_POLICY")
    {
        return AdminPolicy::load_from_path(std::path::Path::new(&test_path), None);
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
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("boreal-admin requires an interactive terminal".into());
    }
    println!("Boreal restricted admin shell. Type help for permitted commands.");

    let mut keys = default_emacs_keybindings();
    keys.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    let menu = ColumnarMenu::default()
        .with_name("completion")
        .with_output_mode(OutputMode::SuggestedSpan);

    let mut editor = Reedline::create()
        .with_completer(Box::new(Completer {
            grammar: Arc::clone(&grammar),
        }))
        .with_highlighter(Box::new(SimpleHighlighter::new(&grammar)))
        .with_hinter(Box::new(DefaultHinter::default()))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_cursor_config(CursorConfig {
            emacs: Some(SetCursorStyle::SteadyBar),
            ..CursorConfig::default()
        })
        .with_edit_mode(Box::new(Emacs::new(keys)));

    let prompt = DefaultPrompt::new(
        DefaultPromptSegment::Basic("boreal-admin".into()),
        DefaultPromptSegment::Empty,
    );

    loop {
        match editor.read_line(&prompt)? {
            Signal::Success(line) => match grammar.parse(&line) {
                Ok(Some(AdminAction::Exit)) => break,
                Ok(Some(action)) => {
                    if let Err(err) = grammar.execute(&action) {
                        eprintln!("error: {err}");
                    }
                }
                Ok(None) => {}
                Err(ParseErr::Quote) => eprintln!("error: unfinished quote or escape"),
                Err(ParseErr::Clap(err)) => err.print()?,
            },
            Signal::CtrlC => continue,
            Signal::CtrlD | Signal::ExternalBreak(_) => break,
            _ => continue,
        }
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn Error>> {
    let grammar = Arc::new(AdminGrammar {
        policy: load_policy()?,
    });
    if let Some(orig_os) = env::var_os("SSH_ORIGINAL_COMMAND") {
        let orig = orig_os
            .into_string()
            .map_err(|_| "SSH command must be UTF-8")?;
        run_single_command(&grammar, &orig)?;
        return Ok(());
    }
    let args: Vec<_> = env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => run_interactive(grammar),
        [flag, cmd] if flag == "-c" => {
            let s = cmd.to_str().ok_or("SSH command must be UTF-8")?;
            run_single_command(&grammar, s)?;
            Ok(())
        }
        _ => Err(
            "unsupported invocation; use interactive boreal-admin or a permitted admin subcommand"
                .into(),
        ),
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
            g.parse("update schedule 0123456789abcdef0123456789ABCDEF01234567")
                .ok()
                .flatten(),
            Some(AdminAction::UpdateSchedule {
                rev: "0123456789abcdef0123456789abcdef01234567".into()
            })
        );

        for rejected in [
            "/bin/sh",
            "cat /home/alice/.config/boreal/secrets/tok",
            "sftp-server",
            "service restart sshd",
            "service status postgresql",
            "services; id",
            "poweroff",
            "sudo -l",
        ] {
            assert!(g.parse(rejected).is_err(), "{rejected}");
        }
    }
}
