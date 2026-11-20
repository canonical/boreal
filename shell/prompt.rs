use std::borrow::Cow;
use std::env;
use std::io::{self, Write};
use std::sync::Arc;

use boreal_common::os;
use boreal_common::wire::PromptSummary;
use crossterm::{queue, style, terminal};
use nu_ansi_term::Color;
use reedline::{PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, PromptViMode};

use crate::helix;

const HOST_ICON: char = '󰇅';
const PAUSED_ICON: char = '';
const RUNNING_ICON: char = '';
const SNOWFLAKE: &str = "\u{2744}\u{fe0f}";

const DRAIN_BADGE: &str = "\x1b[31m󰞴\x1b[0m ";
const OK_INSERT_INDICATOR: &str = "\x1b[1;32m❯\x1b[0m ";
const ERR_INSERT_INDICATOR: &str = "\x1b[1;31m❯\x1b[0m ";
const NORMAL_MODAL_INDICATOR: &str = "\x1b[1;36m❮\x1b[0m ";
const SELECT_MODAL_INDICATOR: &str = "\x1b[1;35m❮\x1b[0m ";
const MULTILINE_INDICATOR: &str = "\x1b[90m∙\x1b[0m ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    Live(String),
    Local,
    Mock,
}

impl Host {
    pub fn live_system() -> Self {
        Self::Live(os::hostname())
    }
}

/// Pushes the active window title onto the XTerm title stack, sets the session
/// title via OSC 0 (`\x1b]0;...\x07`), and restores the previous title on drop.
pub struct TitleGuard {
    active: bool,
}

impl TitleGuard {
    pub fn enter<S: AsRef<str>>(title: S) -> Self {
        if env::var_os("TERM").is_some_and(|t| t == "dumb") {
            return Self { active: false };
        }
        let mut out = io::stdout().lock();
        let _ = write_title_enter(&mut out, title.as_ref());
        Self { active: true }
    }
}

impl Drop for TitleGuard {
    fn drop(&mut self) {
        if self.active {
            let mut out = io::stdout().lock();
            let _ = write_title_exit(&mut out);
        }
    }
}

pub fn write_title_enter<W: Write>(writer: &mut W, title: &str) -> io::Result<()> {
    let safe: String = title.chars().filter(|c| !c.is_control()).collect();
    writer.write_all(b"\x1b[22;0t")?;
    queue!(writer, terminal::SetTitle(safe))?;
    writer.flush()
}

pub fn write_title_exit<W: Write>(writer: &mut W) -> io::Result<()> {
    queue!(writer, terminal::SetTitle(""))?;
    writer.write_all(b"\x1b[23;0t")?;
    writer.flush()
}

/// Two-line Starship-style prompt pre-rendered once per command cycle.
pub struct Prompt {
    window_title: String,
    prefix: String,
    deployment_suffix: String,
    rendered_left: String,
    last_ok: bool,
    helix_state: Option<Arc<helix::State>>,
}

impl Prompt {
    pub fn new(
        caller: &str,
        host: Host,
        deployment: &str,
        helix_state: Option<Arc<helix::State>>,
    ) -> Self {
        let clean_caller: String = caller.chars().filter(|c| !c.is_control()).collect();
        let host_label = match &host {
            Host::Live(name) => os::sanitize_hostname(name),
            Host::Local => String::from("local"),
            Host::Mock => String::from("mock"),
        };
        let window_title = format!("Boreal {clean_caller}@{host_label}");
        let user_seg = Color::Cyan.paint(&clean_caller);
        let host_seg = Color::Purple.paint(format!("{HOST_ICON} {host_label}"));
        let prefix = format!("{user_seg} on {host_seg}");
        let deployment_suffix = format!(" is {SNOWFLAKE} {}", Color::Cyan.paint(deployment));
        let rendered_left = format!("{prefix}{deployment_suffix}\n");
        Self {
            window_title,
            prefix,
            deployment_suffix,
            rendered_left,
            last_ok: true,
            helix_state,
        }
    }

    pub fn window_title(&self) -> &str {
        &self.window_title
    }

    pub fn refresh(&mut self, summary: Option<PromptSummary>, last_ok: bool) {
        self.last_ok = last_ok;
        let mut line = self.prefix.clone();

        if let Some(counts) = summary.and_then(|s| s.workflows)
            && (counts.paused > 0 || counts.running > 0)
        {
            line.push(' ');
            line.push_str(&Color::DarkGray.paint("[").to_string());
            if counts.paused > 0 {
                line.push_str(
                    &Color::LightBlue
                        .paint(format!("{PAUSED_ICON} {}", counts.paused))
                        .to_string(),
                );
                if counts.running > 0 {
                    line.push(' ');
                }
            }
            if counts.running > 0 {
                line.push_str(
                    &Color::LightGreen
                        .paint(format!("{RUNNING_ICON} {}", counts.running))
                        .to_string(),
                );
            }
            line.push_str(&Color::DarkGray.paint("]").to_string());
        }

        line.push_str(&self.deployment_suffix);
        line.push('\n');
        if summary.is_some_and(|s| s.draining) {
            line.push_str(DRAIN_BADGE);
        }
        self.rendered_left = line;
    }
}

impl reedline::Prompt for Prompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.rendered_left)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, edit_mode: PromptEditMode) -> Cow<'_, str> {
        match edit_mode {
            PromptEditMode::Default
            | PromptEditMode::Emacs
            | PromptEditMode::Vi(PromptViMode::Insert) => {
                if self.last_ok {
                    Cow::Borrowed(OK_INSERT_INDICATOR)
                } else {
                    Cow::Borrowed(ERR_INSERT_INDICATOR)
                }
            }
            PromptEditMode::Vi(PromptViMode::Normal) => Cow::Borrowed(NORMAL_MODAL_INDICATOR),
            PromptEditMode::Vi(PromptViMode::Visual) => {
                match self.helix_state.as_deref().map(helix::State::get) {
                    Some(helix::Mode::Select) => Cow::Borrowed(SELECT_MODAL_INDICATOR),
                    _ => Cow::Borrowed(NORMAL_MODAL_INDICATOR),
                }
            }
            PromptEditMode::Custom(name) => Cow::Owned(format!("({name}) ❯ ")),
        }
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(MULTILINE_INDICATOR)
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({prefix}reverse-search: {}) ",
            history_search.term
        ))
    }

    fn get_prompt_color(&self) -> style::Color {
        style::Color::Reset
    }

    fn get_indicator_color(&self) -> style::Color {
        style::Color::Reset
    }

    fn get_prompt_right_color(&self) -> style::Color {
        style::Color::Reset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boreal_common::wire::WorkflowCounts;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use reedline::{EditMode as _, Prompt as _, ReedlineRawEvent, default_vi_insert_keybindings};

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for esc_ch in chars.by_ref() {
                    if ('@'..='~').contains(&esc_ch) {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn test_static_indicator_ansi_sequences_match_palette() {
        assert_eq!(DRAIN_BADGE, format!("{} ", Color::Red.paint("󰞴")));
        assert_eq!(
            OK_INSERT_INDICATOR,
            format!("{} ", Color::Green.bold().paint("❯"))
        );
        assert_eq!(
            ERR_INSERT_INDICATOR,
            format!("{} ", Color::Red.bold().paint("❯"))
        );
        assert_eq!(
            NORMAL_MODAL_INDICATOR,
            format!("{} ", Color::Cyan.bold().paint("❮"))
        );
        assert_eq!(
            SELECT_MODAL_INDICATOR,
            format!("{} ", Color::Purple.bold().paint("❮"))
        );
    }

    #[test]
    fn test_status_line_segments_and_drain_badge() {
        let mut live = Prompt::new("alice", Host::Live("demo-lab\n".into()), "d9c4975", None);
        assert_eq!(
            strip_ansi(&live.render_prompt_left()),
            "alice on 󰇅 demo-lab is ❄️ d9c4975\n"
        );

        let cases = [
            (None, "alice on 󰇅 demo-lab is ❄️ d9c4975\n"),
            (
                Some(PromptSummary {
                    workflows: Some(WorkflowCounts {
                        paused: 0,
                        running: 0,
                    }),
                    draining: false,
                }),
                "alice on 󰇅 demo-lab is ❄️ d9c4975\n",
            ),
            (
                Some(PromptSummary {
                    workflows: Some(WorkflowCounts {
                        paused: 1,
                        running: 0,
                    }),
                    draining: false,
                }),
                "alice on 󰇅 demo-lab [ 1] is ❄️ d9c4975\n",
            ),
            (
                Some(PromptSummary {
                    workflows: Some(WorkflowCounts {
                        paused: 0,
                        running: 2,
                    }),
                    draining: false,
                }),
                "alice on 󰇅 demo-lab [ 2] is ❄️ d9c4975\n",
            ),
            (
                Some(PromptSummary {
                    workflows: Some(WorkflowCounts {
                        paused: 1,
                        running: 2,
                    }),
                    draining: false,
                }),
                "alice on 󰇅 demo-lab [ 1  2] is ❄️ d9c4975\n",
            ),
            (
                Some(PromptSummary {
                    workflows: Some(WorkflowCounts {
                        paused: 1,
                        running: 2,
                    }),
                    draining: true,
                }),
                "alice on 󰇅 demo-lab [ 1  2] is ❄️ d9c4975\n󰞴 ",
            ),
            (
                Some(PromptSummary {
                    workflows: None,
                    draining: true,
                }),
                "alice on 󰇅 demo-lab is ❄️ d9c4975\n󰞴 ",
            ),
        ];
        for (summary, expected) in cases {
            live.refresh(summary, true);
            assert_eq!(
                strip_ansi(&live.render_prompt_left()),
                expected,
                "{summary:?}"
            );
        }

        live.refresh(None, false);
        assert_eq!(
            live.render_prompt_indicator(PromptEditMode::Emacs),
            ERR_INSERT_INDICATOR
        );

        let tagged = Prompt::new("admin", Host::Live("demo-lab".into()), "v0.4.2", None);
        assert_eq!(
            strip_ansi(&tagged.render_prompt_left()),
            "admin on 󰇅 demo-lab is ❄️ v0.4.2\n"
        );
        assert_eq!(live.window_title(), "Boreal alice@demo-lab");
        assert_eq!(tagged.window_title(), "Boreal admin@demo-lab");
    }

    #[test]
    fn test_window_title_osc_sequences_strip_controls_and_restore_stack() {
        let mut enter_buf = Vec::new();
        write_title_enter(&mut enter_buf, "Boreal alice@demo-lab\x07\x1b]9;bad").unwrap();
        assert_eq!(
            enter_buf,
            b"\x1b[22;0t\x1b]0;Boreal alice@demo-lab]9;bad\x07"
        );

        let mut exit_buf = Vec::new();
        write_title_exit(&mut exit_buf).unwrap();
        assert_eq!(exit_buf, b"\x1b]0;\x07\x1b[23;0t");
    }

    #[test]
    fn test_vi_and_helix_modal_indicators() {
        let hx_state = Arc::new(helix::State::new());
        let mut hx =
            helix::Helix::with_state(default_vi_insert_keybindings(), Arc::clone(&hx_state));
        let prompt = Prompt::new("bob", Host::Mock, "unknown", Some(hx_state));

        assert_eq!(
            prompt.render_prompt_indicator(hx.edit_mode()),
            OK_INSERT_INDICATOR
        );

        let esc =
            ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
                .unwrap();
        hx.parse_event(esc);
        assert_eq!(
            prompt.render_prompt_indicator(hx.edit_mode()),
            NORMAL_MODAL_INDICATOR
        );

        let select = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        hx.parse_event(select);
        assert_eq!(
            prompt.render_prompt_indicator(hx.edit_mode()),
            SELECT_MODAL_INDICATOR
        );

        assert_eq!(
            prompt.render_prompt_indicator(PromptEditMode::Vi(PromptViMode::Normal)),
            NORMAL_MODAL_INDICATOR
        );
    }
}
