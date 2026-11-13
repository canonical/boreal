mod command;
mod completion;
mod highlighter;

use std::{error::Error, io::IsTerminal};

use crossterm::cursor::SetCursorStyle;
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, CursorConfig, DefaultHinter, DefaultPrompt, DefaultPromptSegment, Emacs,
    InputMode, KeyCode, KeyModifiers, MenuBuilder, OutputMode, Reedline, ReedlineEvent,
    ReedlineMenu, Signal, default_emacs_keybindings,
};

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args_os().len() != 1 {
        return Err("this prototype accepts no command-line arguments".into());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("this prototype requires an interactive terminal".into());
    }

    println!("Boreal shell prototype: commands are mock-only and perform no actions.");
    println!("Type help for commands; history is kept in memory for this session.");

    let mut keys = default_emacs_keybindings();
    keys.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    let selected = Style::new().fg(Color::Default).on(Color::Default).reverse();
    let menu = Box::new(
        ColumnarMenu::default()
            .with_name("completion")
            .with_input_mode(InputMode::FullBuffer)
            .with_output_mode(OutputMode::SuggestedSpan)
            .with_text_style(Color::DarkGray.normal())
            .with_match_text_style(Color::DarkGray.normal().underline())
            .with_selected_text_style(selected)
            .with_selected_match_text_style(selected.underline()),
    );
    let mut editor = Reedline::create()
        .with_completer(Box::new(completion::Completer))
        .with_highlighter(Box::new(highlighter::Highlighter::new()))
        .with_hinter(Box::new(DefaultHinter::default()))
        .with_menu(ReedlineMenu::EngineCompleter(menu))
        .with_cursor_config(CursorConfig {
            emacs: Some(SetCursorStyle::SteadyBar),
            ..CursorConfig::default()
        })
        .with_edit_mode(Box::new(Emacs::new(keys)));
    let prompt = DefaultPrompt::new(
        DefaultPromptSegment::Basic("boreal".into()),
        DefaultPromptSegment::Empty,
    );

    loop {
        match editor.read_line(&prompt)? {
            Signal::Success(line) => match command::parse(&line) {
                Ok(Some(command::Action::Exit)) => break,
                Ok(Some(action)) => println!("[mock] {action:?} (no action performed)"),
                Ok(None) => {}
                Err(command::Error::Quote) => eprintln!("error: unfinished quote or escape"),
                Err(command::Error::Clap(error)) => error.print()?,
            },
            Signal::CtrlC => continue,
            Signal::CtrlD | Signal::ExternalBreak(_) => break,
            _ => continue,
        }
    }
    Ok(())
}
