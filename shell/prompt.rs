use std::borrow::Cow;
use std::env;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use boreal_common::os;
use boreal_common::wire::PromptSummary;
use crossterm::{queue, style, terminal};
use nu_ansi_term::Color;
use reedline::{PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, PromptViMode};

use crate::helix;

const DURATION_THRESHOLD: Duration = Duration::from_secs(1);

const OK_INSERT_INDICATOR: &str = "\x1b[1;32m❯\x1b[0m ";
const ERR_INSERT_INDICATOR: &str = "\x1b[1;31m❯\x1b[0m ";
const NORMAL_MODAL_INDICATOR: &str = "\x1b[1;36m❮\x1b[0m ";
const SELECT_MODAL_INDICATOR: &str = "\x1b[1;35m❮\x1b[0m ";
const MULTILINE_INDICATOR: &str = "\x1b[90m∙\x1b[0m ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    Live(String),
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
    identity: String,
    rendered_left: String,
    last_ok: bool,
    helix_state: Option<Arc<helix::State>>,
}

impl Prompt {
    pub fn new<S: AsRef<str>>(
        caller: S,
        host: Host,
        helix_state: Option<Arc<helix::State>>,
    ) -> Self {
        let clean_caller: String = caller
            .as_ref()
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        let (host_label, host_seg) = match &host {
            Host::Live(name) => {
                let clean_host = os::sanitize_hostname(name);
                let painted = Color::Blue.bold().paint(clean_host.clone()).to_string();
                (clean_host, painted)
            }
            Host::Mock => (
                String::from("mock"),
                Color::Purple.bold().paint("mock").to_string(),
            ),
        };
        let window_title = format!("Boreal {clean_caller}@{host_label}");
        let user_seg = Color::Cyan.bold().paint(&clean_caller);
        let at_seg = Color::DarkGray.normal().paint("@");
        let identity = format!("{user_seg}{at_seg}{host_seg}");
        let rendered_left = format!("{identity}\n");
        Self {
            window_title,
            identity,
            rendered_left,
            last_ok: true,
            helix_state,
        }
    }

    pub fn window_title(&self) -> &str {
        &self.window_title
    }

    pub fn refresh(
        &mut self,
        summary: Option<PromptSummary>,
        last_ok: bool,
        last_duration: Option<Duration>,
    ) {
        self.last_ok = last_ok;
        let sep = Color::DarkGray.normal().paint(" · ");
        let mut line = self.identity.clone();

        if let Some(sum) = summary {
            if let Some(count) = sum.running_workflows
                && count > 0
            {
                let badge = Color::Green.bold().paint(format!("{count} running"));
                line.push_str(&format!("{sep}{badge}"));
            }
            if sum.draining {
                let badge = Color::Yellow.bold().paint("draining");
                line.push_str(&format!("{sep}{badge}"));
            }
        }

        if let Some(dur_text) = last_duration.and_then(format_duration) {
            let badge = Color::Yellow.normal().paint(format!("took {dur_text}"));
            line.push_str(&format!("{sep}{badge}"));
        }

        line.push('\n');
        self.rendered_left = line;
    }
}

pub fn format_duration(elapsed: Duration) -> Option<String> {
    if elapsed < DURATION_THRESHOLD {
        return None;
    }
    let secs = elapsed.as_secs();
    if secs < 60 {
        let tenths = elapsed.as_millis() / 100;
        Some(format!("{}.{}s", tenths / 10, tenths % 10))
    } else if secs < 3600 {
        Some(format!("{}m {:02}s", secs / 60, secs % 60))
    } else {
        Some(format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60))
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
    fn test_status_line_segments_and_zero_noise_omission() {
        let mut live = Prompt::new("alice", Host::Live("demo-lab\n".into()), None);
        assert_eq!(strip_ansi(&live.render_prompt_left()), "alice@demo-lab\n");

        // Zero running workflows, no drain, and < 1s duration stay silent.
        live.refresh(
            Some(PromptSummary {
                running_workflows: Some(0),
                draining: false,
            }),
            true,
            Some(Duration::from_millis(950)),
        );
        assert_eq!(strip_ansi(&live.render_prompt_left()), "alice@demo-lab\n");

        // Active workflows, draining gate, and slow command render cleanly.
        live.refresh(
            Some(PromptSummary {
                running_workflows: Some(2),
                draining: true,
            }),
            false,
            Some(Duration::from_millis(1420)),
        );
        assert_eq!(
            strip_ansi(&live.render_prompt_left()),
            "alice@demo-lab · 2 running · draining · took 1.4s\n"
        );
        assert_eq!(
            live.render_prompt_indicator(PromptEditMode::Emacs),
            ERR_INSERT_INDICATOR
        );

        // Mock preview renders <user>@mock and matching window title.
        let mock = Prompt::new("alice", Host::Mock, None);
        assert_eq!(strip_ansi(&mock.render_prompt_left()), "alice@mock\n");
        assert_eq!(live.window_title(), "Boreal alice@demo-lab");
        assert_eq!(mock.window_title(), "Boreal alice@mock");
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
    fn test_duration_formatting_tiers() {
        let cases = [
            (Duration::from_millis(0), None),
            (Duration::from_millis(999), None),
            (Duration::from_millis(1000), Some("1.0s")),
            (Duration::from_millis(2490), Some("2.4s")),
            (Duration::from_secs(65), Some("1m 05s")),
            (Duration::from_secs(3725), Some("1h 02m")),
        ];
        for (input, expected) in cases {
            assert_eq!(format_duration(input).as_deref(), expected, "{input:?}");
        }
    }

    #[test]
    fn test_vi_and_helix_modal_indicators() {
        let hx_state = Arc::new(helix::State::new());
        let mut hx =
            helix::Helix::with_state(default_vi_insert_keybindings(), Arc::clone(&hx_state));
        let prompt = Prompt::new("bob", Host::Mock, Some(hx_state));

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
