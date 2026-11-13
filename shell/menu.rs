use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, Completer, Editor, InputMode, MenuBuilder, MenuEvent, MenuTextStyle, OutputMode,
    Painter, Suggestion,
};

pub struct Menu {
    inner: ColumnarMenu,
    selected_prefix: String,
    description_prefix: String,
}

impl Menu {
    pub fn new() -> Self {
        let selected = Style::new().fg(Color::Default).on(Color::Default).reverse();
        Self {
            inner: ColumnarMenu::default()
                .with_name("completion")
                .with_input_mode(InputMode::FullBuffer)
                .with_output_mode(OutputMode::SuggestedSpan)
                .with_text_style(Color::DarkGray.normal())
                .with_match_text_style(Color::DarkGray.normal().underline())
                .with_selected_text_style(selected)
                .with_selected_match_text_style(selected.underline()),
            selected_prefix: selected.prefix().to_string(),
            description_prefix: MenuTextStyle::default()
                .description_style
                .prefix()
                .to_string(),
        }
    }
}

impl reedline::Menu for Menu {
    // MenuSettings is not re-exported by Reedline 0.49. Its engine uses these
    // accessors rather than settings() on the wrapped menu.
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn indicator(&self) -> &str {
        self.inner.indicator()
    }

    fn is_active(&self) -> bool {
        self.inner.is_active()
    }

    fn menu_event(&mut self, event: MenuEvent) {
        self.inner.menu_event(event);
    }

    fn can_quick_complete(&self) -> bool {
        self.inner.can_quick_complete()
    }

    fn can_partially_complete(
        &mut self,
        values_updated: bool,
        editor: &mut Editor,
        completer: &mut dyn Completer,
    ) -> bool {
        self.inner
            .can_partially_complete(values_updated, editor, completer)
    }

    fn update_values(&mut self, editor: &mut Editor, completer: &mut dyn Completer) {
        self.inner.update_values(editor, completer);
    }

    fn update_working_details(
        &mut self,
        editor: &mut Editor,
        completer: &mut dyn Completer,
        painter: &Painter,
    ) {
        self.inner
            .update_working_details(editor, completer, painter);
    }

    fn replace_in_buffer(&self, editor: &mut Editor) {
        self.inner.replace_in_buffer(editor);
    }

    fn menu_required_lines(&self, terminal_columns: u16) -> u16 {
        self.inner.menu_required_lines(terminal_columns)
    }

    fn menu_string(&self, available_lines: u16, use_ansi_coloring: bool) -> String {
        let rendered = self.inner.menu_string(available_lines, use_ansi_coloring);
        if use_ansi_coloring {
            fix_selected_descriptions(&rendered, &self.selected_prefix, &self.description_prefix)
        } else {
            rendered
        }
    }

    fn min_rows(&self) -> u16 {
        self.inner.min_rows()
    }

    fn get_values(&self) -> &[Suggestion] {
        self.inner.get_values()
    }

    fn set_cursor_pos(&mut self, pos: (u16, u16)) {
        self.inner.set_cursor_pos(pos);
    }
}

fn fix_selected_descriptions(rendered: &str, selected: &str, description: &str) -> String {
    // Reedline 0.49 emits selected style, padding, then the yellow description
    // prefix. With reverse active that foreground becomes the background; drop
    // only that prefix, relying on this exact ColumnarMenu rendering pattern.
    let mut result = String::with_capacity(rendered.len());
    let mut remaining = rendered;
    while let Some(index) = remaining.find(selected) {
        let through_prefix = index + selected.len();
        result.push_str(&remaining[..through_prefix]);
        remaining = &remaining[through_prefix..];

        let spaces = remaining.bytes().take_while(|byte| *byte == b' ').count();
        result.push_str(&remaining[..spaces]);
        remaining = &remaining[spaces..];
        if let Some(after_description) = remaining.strip_prefix(description) {
            remaining = after_description;
        }
    }
    result.push_str(remaining);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::{Menu as _, Span, UndoBehavior};

    #[test]
    fn test_selected_description_transform() {
        let menu = Menu::new();
        assert_eq!(menu.selected_prefix, "\x1b[7;49;39m");
        assert_eq!(menu.description_prefix, "\x1b[33m");

        let cases = [
            (
                "selected with padding and underlined match",
                "\x1b[0m\x1b[90m\x1b[4;7;49;39mca\x1b[0m\x1b[90m\x1b[7;49;39mt\x1b[0m\x1b[90m\x1b[7;49;39m  \x1b[33mdescription\x1b[0m\x1b[0m",
                "\x1b[0m\x1b[90m\x1b[4;7;49;39mca\x1b[0m\x1b[90m\x1b[7;49;39mt\x1b[0m\x1b[90m\x1b[7;49;39m  description\x1b[0m\x1b[0m",
            ),
            (
                "inactive yellow remains",
                "\x1b[90mother  \x1b[0m\x1b[33mdescription\x1b[0m",
                "\x1b[90mother  \x1b[0m\x1b[33mdescription\x1b[0m",
            ),
            (
                "unicode candidate and description",
                "🐈\x1b[7;49;39m猫  \x1b[0m\x1b[7;49;39m \x1b[33m説明🌍\x1b[0m",
                "🐈\x1b[7;49;39m猫  \x1b[0m\x1b[7;49;39m 説明🌍\x1b[0m",
            ),
            (
                "zero padding",
                "\x1b[7;49;39m\x1b[33m説明\x1b[0m",
                "\x1b[7;49;39m説明\x1b[0m",
            ),
            (
                "candidate and other sequences remain",
                "\x1b[7;49;39mhello  \x1b[33mword\x1b[0m\x1b[7;49;39m \x1b[1m\x1b[33mother\x1b[0m",
                "\x1b[7;49;39mhello  \x1b[33mword\x1b[0m\x1b[7;49;39m \x1b[1m\x1b[33mother\x1b[0m",
            ),
            (
                "multiple entries",
                "\x1b[7;49;39m \x1b[33mone\x1b[0m\r\n\x1b[7;49;39m  \x1b[33mtwo\x1b[0m",
                "\x1b[7;49;39m one\x1b[0m\r\n\x1b[7;49;39m  two\x1b[0m",
            ),
            ("no ANSI", ">猫  説明\r\n", ">猫  説明\r\n"),
        ];
        for (name, input, expected) in cases {
            assert_eq!(
                fix_selected_descriptions(input, &menu.selected_prefix, &menu.description_prefix),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn test_menu_delegates_completion_and_events() {
        struct TestCompleter {
            input: Option<(String, usize)>,
        }

        impl Completer for TestCompleter {
            fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
                self.input = Some((line.to_owned(), pos));
                vec![Suggestion {
                    value: "replacement".into(),
                    span: Span { start: 0, end: 3 },
                    ..Suggestion::default()
                }]
            }
        }

        let mut menu = Menu::new();
        assert_eq!(menu.name(), "completion");
        assert_eq!(menu.indicator(), menu.inner.indicator());
        assert!(!menu.is_active());
        assert!(menu.can_quick_complete());
        assert_eq!(menu.menu_string(5, false), menu.inner.menu_string(5, false));
        menu.menu_event(MenuEvent::Activate(false));
        assert!(menu.is_active());

        let mut editor = Editor::default();
        editor.edit_buffer(
            |buffer| {
                buffer.set_buffer("abc tail".into());
                buffer.set_insertion_point(3);
            },
            UndoBehavior::CreateUndoPoint,
        );
        let mut completer = TestCompleter { input: None };
        menu.update_values(&mut editor, &mut completer);
        assert_eq!(completer.input, Some(("abc tail".into(), 3)));
        assert_eq!(menu.get_values()[0].value, "replacement");
        menu.replace_in_buffer(&mut editor);
        assert_eq!(editor.get_buffer(), "replacement tail");
        menu.menu_event(MenuEvent::Deactivate);
        assert!(!menu.is_active());
    }
}
