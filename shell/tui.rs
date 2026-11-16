use std::io::{self, Write};
use std::time::{Duration, Instant};

use boreal_common::wire::{
    Request, Response, ResultOutcome, StepState, WorkflowStep, WorkflowView,
};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, execute, queue};
use nu_ansi_term::{Color, Style};

use crate::client::Mode;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_POLL: Duration = Duration::from_millis(1000);
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Steps,
    Events,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Refresh,
    Quit,
}

type Span = (Style, String);
type Line = Vec<Span>;

pub struct State {
    view: WorkflowView,
    pane: Pane,
    step_scroll: usize,
    event_scroll: usize,
    follow: bool,
    tick: usize,
    notice: String,
}

impl State {
    pub fn new(view: WorkflowView) -> Self {
        Self {
            view,
            pane: Pane::Events,
            step_scroll: 0,
            event_scroll: usize::MAX,
            follow: true,
            tick: 0,
            notice: String::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.view.status.as_str(), "running" | "paused")
    }

    pub fn update(&mut self, next: WorkflowView) {
        self.view = next;
        self.tick = self.tick.wrapping_add(1);
        self.notice.clear();
        if self.follow {
            self.event_scroll = usize::MAX;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, page_step: usize) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Outcome::Quit,
            KeyCode::Char('c' | 'd') if ctrl => Outcome::Quit,
            KeyCode::Tab | KeyCode::BackTab => {
                self.pane = match self.pane {
                    Pane::Steps => Pane::Events,
                    Pane::Events => Pane::Steps,
                };
                Outcome::Continue
            }
            KeyCode::Char('1') => {
                self.pane = Pane::Steps;
                Outcome::Continue
            }
            KeyCode::Char('2') => {
                self.pane = Pane::Events;
                Outcome::Continue
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                if self.follow {
                    self.event_scroll = usize::MAX;
                }
                Outcome::Continue
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_by(1, true);
                Outcome::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_by(1, false);
                Outcome::Continue
            }
            KeyCode::PageDown => {
                self.scroll_by(page_step.max(1), true);
                Outcome::Continue
            }
            KeyCode::PageUp => {
                self.scroll_by(page_step.max(1), false);
                Outcome::Continue
            }
            KeyCode::Home | KeyCode::Char('g') => {
                match self.pane {
                    Pane::Steps => self.step_scroll = 0,
                    Pane::Events => {
                        self.follow = false;
                        self.event_scroll = 0;
                    }
                }
                Outcome::Continue
            }
            KeyCode::End | KeyCode::Char('G') => {
                match self.pane {
                    Pane::Steps => self.step_scroll = usize::MAX,
                    Pane::Events => {
                        self.follow = true;
                        self.event_scroll = usize::MAX;
                    }
                }
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn scroll_by(&mut self, delta: usize, down: bool) {
        match self.pane {
            Pane::Steps => {
                self.step_scroll = if down {
                    self.step_scroll.saturating_add(delta)
                } else {
                    self.step_scroll.saturating_sub(delta)
                };
            }
            Pane::Events => {
                self.follow = false;
                self.event_scroll = if down {
                    self.event_scroll.saturating_add(delta)
                } else {
                    self.event_scroll.saturating_sub(delta)
                };
            }
        }
    }
}

struct Guard;

impl Guard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(err) = execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(err);
        }
        Ok(Self)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

pub fn run(initial: WorkflowView, owner: &str, id: &str, mode: &Mode) -> Result<(), String> {
    let _guard = Guard::enter().map_err(|e| format!("enter TUI: {e}"))?;
    let mut state = State::new(initial);
    let req = Request::View {
        owner: owner.to_owned(),
        id: id.to_owned(),
    };
    let mut next_poll = Instant::now() + POLL_INTERVAL;

    loop {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let frame = render_frame(&mut state, cols, rows, mode.is_mock());
        let mut out = io::stdout().lock();
        out.write_all(&frame).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        drop(out);

        let wait = if state.is_open() {
            next_poll.saturating_duration_since(Instant::now())
        } else {
            IDLE_POLL
        };

        let mut force_refresh = false;
        if event::poll(wait).map_err(|e| e.to_string())? {
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) => {
                    let page = (rows.saturating_sub(6) / 2).max(1) as usize;
                    match state.handle_key(key, page) {
                        Outcome::Quit => break,
                        Outcome::Refresh => force_refresh = true,
                        Outcome::Continue => {}
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if force_refresh || (state.is_open() && Instant::now() >= next_poll) {
            match mode.send(&req) {
                Ok(Response::View(v)) => state.update(v),
                Ok(Response::Error { message, .. }) => state.notice = sanitize_inline(&message),
                Ok(_) => {}
                Err(err) => state.notice = sanitize_inline(&err),
            }
            next_poll = Instant::now() + POLL_INTERVAL;
        }
    }
    Ok(())
}

pub fn sanitize_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\t' {
            out.push_str("  ");
        } else if matches!(ch, '\n' | '\r') {
            out.push(' ');
        } else if !ch.is_control() {
            out.push(ch);
        }
    }
    out
}

fn status_badge(status: &str, tick: usize) -> Span {
    let clean = sanitize_inline(status);
    match clean.as_str() {
        "running" => {
            let spin = SPINNER[tick % SPINNER.len()];
            (Color::LightGreen.bold(), format!("{spin} running"))
        }
        "paused" => (Color::LightBlue.bold(), " paused".into()),
        "completed" => (Color::Green.bold(), "✓ completed".into()),
        "cancelled" => (Color::Yellow.bold(), "⊘ cancelled".into()),
        "failed" | "terminated" | "timed_out" => (Color::Red.bold(), format!("✗ {clean}")),
        _ => (Color::DarkGray.normal(), format!("• {clean}")),
    }
}

fn step_icon(state: StepState) -> Span {
    match state {
        StepState::Pending => (Color::DarkGray.normal(), "○".into()),
        StepState::Running => (Color::LightGreen.bold(), "●".into()),
        StepState::Retrying => (Color::Yellow.bold(), "↻".into()),
        StepState::Completed => (Color::Green.bold(), "✓".into()),
        StepState::Failed | StepState::TimedOut => (Color::Red.bold(), "✗".into()),
        StepState::Cancelled => (Color::Yellow.normal(), "⊘".into()),
    }
}

fn format_step_row(step: &WorkflowStep) -> Line {
    let mut row = vec![(Style::new(), "  ".into()), step_icon(step.state)];
    row.push((
        Color::DarkGray.normal(),
        format!(" {} ", sanitize_inline(&step.kind)),
    ));
    row.push((Style::new(), sanitize_inline(&step.name)));
    if !step.duration.is_empty() {
        row.push((
            Color::Cyan.normal(),
            format!("  {}", sanitize_inline(&step.duration)),
        ));
    }
    if !step.detail.is_empty() {
        let detail_style = match step.state {
            StepState::Failed | StepState::TimedOut => Color::Red.normal(),
            StepState::Retrying => Color::Yellow.normal(),
            _ => Color::DarkGray.normal(),
        };
        row.push((
            detail_style,
            format!("  ({})", sanitize_inline(&step.detail)),
        ));
    }
    row
}

fn build_step_lines(view: &WorkflowView) -> Vec<Line> {
    let mut lines = Vec::new();
    if !view.input.is_empty() {
        lines.push(vec![
            (Color::DarkGray.normal(), "  input: ".into()),
            (Style::new(), sanitize_inline(&view.input)),
        ]);
    }
    if view.steps.is_empty() {
        lines.push(vec![(
            Color::DarkGray.normal(),
            "  (no steps recorded yet)".into(),
        )]);
    } else {
        for step in &view.steps {
            lines.push(format_step_row(step));
        }
    }
    if let Some(outcome) = &view.outcome {
        match outcome {
            ResultOutcome::Completed { output, .. } => {
                lines.push(vec![(Color::Green.bold(), "  result:".into())]);
                if output.is_empty() {
                    lines.push(vec![(Color::DarkGray.normal(), "    (empty)".into())]);
                } else {
                    for raw_line in output.lines() {
                        lines.push(vec![(
                            Style::new(),
                            format!("    {}", sanitize_inline(raw_line)),
                        )]);
                    }
                }
            }
            ResultOutcome::Unsuccessful { status, detail, .. } => {
                lines.push(vec![
                    (
                        Color::Red.bold(),
                        format!("  {}: ", sanitize_inline(status)),
                    ),
                    (Color::Red.normal(), sanitize_inline(detail)),
                ]);
            }
            ResultOutcome::Running { .. } => {}
        }
    }
    lines
}

fn build_event_lines(view: &WorkflowView) -> Vec<Line> {
    if view.events.is_empty() {
        return vec![vec![(
            Color::DarkGray.normal(),
            "  (waiting for workflow events...)".into(),
        )]];
    }
    view.events
        .iter()
        .map(|ev| {
            let kind_clean = sanitize_inline(&ev.kind);
            let kind_style = if kind_clean.contains("Failed")
                || kind_clean.contains("TimedOut")
                || kind_clean.contains("Terminated")
            {
                Color::Red.normal()
            } else if kind_clean.contains("Completed") || kind_clean.contains("Fired") {
                Color::Green.normal()
            } else if kind_clean.contains("Cancel") {
                Color::Yellow.normal()
            } else {
                Color::Cyan.normal()
            };
            vec![
                (
                    Color::DarkGray.normal(),
                    format!("  [{}] #{:<3} ", sanitize_inline(&ev.timestamp), ev.id),
                ),
                (kind_style, format!("{kind_clean:<22}")),
                (Style::new(), format!(" {}", sanitize_inline(&ev.summary))),
            ]
        })
        .collect()
}

fn section_bar(title: &str, active: bool, suffix: &str, max_cols: usize) -> Line {
    let lead = if active { "▸ " } else { "  " };
    let head = format!("{lead}{title}");
    let tail = if suffix.is_empty() {
        String::new()
    } else {
        format!(" {suffix} ")
    };
    let used = head.chars().count() + tail.chars().count() + 1;
    let dashes = "─".repeat(max_cols.saturating_sub(used));
    let head_style = if active {
        Color::Cyan.bold()
    } else {
        Color::DarkGray.bold()
    };
    vec![
        (head_style, format!("{head} ")),
        (Color::DarkGray.normal(), tail),
        (Color::DarkGray.normal(), dashes),
    ]
}

fn write_clipped_row(buf: &mut Vec<u8>, row: u16, spans: &[Span], max_cols: usize) {
    let _ = queue!(
        buf,
        cursor::MoveTo(0, row),
        terminal::Clear(ClearType::CurrentLine)
    );
    let limit = max_cols.saturating_sub(2).max(1);
    let mut remaining = limit;
    for (style, text) in spans {
        if remaining == 0 {
            break;
        }
        let count = text.chars().count();
        if count <= remaining {
            let _ = write!(buf, "{}", style.paint(text));
            remaining -= count;
        } else {
            let mut clipped: String = text.chars().take(remaining.saturating_sub(1)).collect();
            clipped.push('…');
            let _ = write!(buf, "{}", style.paint(clipped));
            remaining = 0;
        }
    }
}

pub fn render_frame(state: &mut State, cols: u16, rows: u16, mock: bool) -> Vec<u8> {
    let width = cols.max(20) as usize;
    let total_rows = rows.max(8) as usize;
    let mut buf = Vec::with_capacity(4096);

    let v = &state.view;
    let mut hdr0 = Vec::new();
    if mock {
        hdr0.push((Color::Purple.normal(), "[mock] ".into()));
    }
    hdr0.push(status_badge(&v.status, state.tick));
    hdr0.push((
        Color::Cyan.bold(),
        format!(
            "  {}/{}",
            sanitize_inline(&v.owner),
            sanitize_inline(&v.worker)
        ),
    ));
    hdr0.push((
        Color::DarkGray.normal(),
        format!(" ({}) ", sanitize_inline(&v.workflow_type)),
    ));
    hdr0.push((
        Style::new().bold(),
        format!("'{}'", sanitize_inline(&v.workflow_id)),
    ));
    write_clipped_row(&mut buf, 0, &hdr0, width);

    let mut hdr1 = vec![
        (Color::DarkGray.normal(), "Run: ".into()),
        (Style::new(), sanitize_inline(&v.run_id)),
        (Color::DarkGray.normal(), "  Started: ".into()),
        (Style::new(), sanitize_inline(&v.start_time)),
        (Color::DarkGray.normal(), "  Duration: ".into()),
        (Color::Cyan.normal(), sanitize_inline(&v.elapsed)),
    ];
    if !v.task_warning.is_empty() {
        hdr1.push((
            Color::Red.bold(),
            format!("  ! {}", sanitize_inline(&v.task_warning)),
        ));
    }
    write_clipped_row(&mut buf, 1, &hdr1, width);

    let avail = total_rows.saturating_sub(5);
    let step_rows = (avail * 2 / 5).max(1);
    let event_rows = avail.saturating_sub(step_rows).max(1);

    let step_lines = build_step_lines(v);
    let max_step_scroll = step_lines.len().saturating_sub(step_rows);
    state.step_scroll = state.step_scroll.min(max_step_scroll);

    let step_bar = section_bar(
        "STEPS & IO",
        state.pane == Pane::Steps,
        &format!("({} steps)", v.steps.len()),
        width,
    );
    write_clipped_row(&mut buf, 2, &step_bar, width);

    for i in 0..step_rows {
        let row_idx = (3 + i) as u16;
        if let Some(line) = step_lines.get(state.step_scroll + i) {
            write_clipped_row(&mut buf, row_idx, line, width);
        } else {
            write_clipped_row(&mut buf, row_idx, &[], width);
        }
    }

    let event_lines = build_event_lines(v);
    let max_event_scroll = event_lines.len().saturating_sub(event_rows);
    if state.follow {
        state.event_scroll = max_event_scroll;
    } else {
        state.event_scroll = state.event_scroll.min(max_event_scroll);
    }

    let follow_tag = if state.follow {
        "follow: ON"
    } else {
        "follow: OFF"
    };
    let ev_bar_row = (3 + step_rows) as u16;
    let ev_bar = section_bar(
        "LIVE EVENT LOG",
        state.pane == Pane::Events,
        &format!("[{follow_tag}] ({} events)", v.events.len()),
        width,
    );
    write_clipped_row(&mut buf, ev_bar_row, &ev_bar, width);

    for i in 0..event_rows {
        let row_idx = ev_bar_row + 1 + (i as u16);
        if row_idx >= (total_rows - 1) as u16 {
            break;
        }
        if let Some(line) = event_lines.get(state.event_scroll + i) {
            write_clipped_row(&mut buf, row_idx, line, width);
        } else {
            write_clipped_row(&mut buf, row_idx, &[], width);
        }
    }

    let footer_row = (total_rows - 1) as u16;
    let help = " q/Esc: quit │ Tab: pane │ j/k/↑/↓/PgUp/PgDn/g/G: scroll │ f: follow │ r: refresh";
    let mut footer = vec![(Color::DarkGray.normal(), help.into())];
    if !state.notice.is_empty() {
        footer.push((Color::Red.normal(), format!(" │ {}", state.notice)));
    }
    write_clipped_row(&mut buf, footer_row, &footer, width);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use boreal_common::wire::WorkflowEvent;

    fn sample_view() -> WorkflowView {
        WorkflowView {
            owner: "alice".into(),
            worker: "greet".into(),
            workflow_type: "DemoWorkflow".into(),
            workflow_id: "job-1\x1b]9;evil\x07".into(),
            run_id: "run-123".into(),
            status: "running".into(),
            start_time: "2026-11-16 14:02:10 UTC".into(),
            close_time: "-".into(),
            elapsed: "3.10s".into(),
            input: r#"{"name":"Ada"}"#.into(),
            task_warning: String::new(),
            steps: vec![WorkflowStep {
                kind: "activity".into(),
                name: "greet".into(),
                state: StepState::Retrying,
                duration: "1.20s elapsed".into(),
                detail: "attempt 2/5 · heartbeat: 50%".into(),
            }],
            events: (1..=20)
                .map(|id| WorkflowEvent {
                    id,
                    timestamp: "14:02:10.000".into(),
                    kind: "ActivityStarted".into(),
                    summary: format!("step-{id}"),
                })
                .collect(),
            outcome: None,
        }
    }

    #[test]
    fn test_render_frame_strips_control_injections_and_tracks_scroll_keys() {
        let mut state = State::new(sample_view());
        let raw = String::from_utf8(render_frame(&mut state, 100, 24, true)).unwrap();
        assert!(raw.contains("DemoWorkflow"));
        assert!(raw.contains("job-1]9;evil"));
        assert!(!raw.contains("\x1b]9;evil\x07"));
        assert!(raw.contains("heartbeat: 50%"));
        assert!(raw.contains("step-20"));

        // Scrolling up disables auto-follow and reveals the top event on 'g'.
        assert!(state.follow);
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), 5),
            Outcome::Continue
        );
        assert!(!state.follow);
        state.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), 5);
        let top_raw = String::from_utf8(render_frame(&mut state, 100, 24, false)).unwrap();
        assert!(top_raw.contains("step-1"));

        // 'f' re-enables follow; Tab switches panes; 'q' quits.
        state.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), 5);
        assert!(state.follow);
        state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 5);
        assert_eq!(state.pane, Pane::Steps);
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 5),
            Outcome::Quit
        );
    }
}
