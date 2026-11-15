use std::sync::Arc;
use std::{iter::Peekable, str::CharIndices};

use nu_ansi_term::{Color, Style};
use reedline::{Completer as _, StyledText};

use crate::command::Context;
use crate::completion::Completer;

/// Highlights the shell's command words and option-like arguments.
pub struct Highlighter {
    ctx: Arc<Context>,
    commands: Vec<String>,
}

impl Highlighter {
    pub fn new(ctx: Arc<Context>) -> Self {
        let mut cli = ctx.cli(None);
        cli.build();
        let commands = cli
            .get_subcommands()
            .flat_map(|command| {
                std::iter::once(command.get_name()).chain(command.get_all_aliases())
            })
            .map(str::to_owned)
            .collect();
        Self { ctx, commands }
    }
}

impl reedline::Highlighter for Highlighter {
    fn highlight(&self, line: &str, cursor: usize) -> StyledText {
        let mut text = StyledText::new();
        let mut words = Words::new(line);
        let mut painted = 0;

        if let Some(command) = words.next() {
            let is_option = command.value.starts_with('-') && command.value.len() > 1;
            let style = if is_option {
                Style::new().fg(Color::Yellow)
            } else if self.commands.iter().any(|name| name == &command.value) {
                Style::new().fg(Color::Green)
            } else if !command.value.is_empty()
                && (self
                    .commands
                    .iter()
                    .any(|name| name.starts_with(&command.value))
                    || (command.start..=command.end).contains(&cursor)
                        && Completer::new(Arc::clone(&self.ctx))
                            .complete(line, cursor)
                            .iter()
                            .any(|candidate| {
                                self.commands
                                    .iter()
                                    .any(|name| name == candidate.display_value())
                            }))
            {
                Style::new().underline()
            } else if !command.value.is_empty() {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            paint(
                &mut text,
                line,
                &mut painted,
                command.start,
                if is_option {
                    line[command.start..command.end]
                        .find('=')
                        .map_or(command.end, |offset| command.start + offset)
                } else {
                    command.end
                },
                style,
            );

            let mut options = command.value != "--";
            for word in words {
                if options && word.value == "--" {
                    paint(
                        &mut text,
                        line,
                        &mut painted,
                        word.start,
                        word.end,
                        Style::new().fg(Color::Yellow),
                    );
                    options = false;
                } else if options && word.value.starts_with('-') && word.value.len() > 1 {
                    let end = line[word.start..word.end]
                        .find('=')
                        .map_or(word.end, |offset| word.start + offset);
                    paint(
                        &mut text,
                        line,
                        &mut painted,
                        word.start,
                        end,
                        Style::new().fg(Color::Yellow),
                    );
                }
            }
        }
        append(&mut text, Style::new(), &line[painted..]);
        text
    }
}

fn paint(
    text: &mut StyledText,
    line: &str,
    painted: &mut usize,
    start: usize,
    end: usize,
    style: Style,
) {
    append(text, Style::new(), &line[*painted..start]);
    append(text, style, &line[start..end]);
    *painted = end;
}

fn append(text: &mut StyledText, style: Style, part: &str) {
    if part.is_empty() {
        return;
    }
    if let Some((previous_style, previous)) = text.buffer.last_mut()
        && *previous_style == style
    {
        previous.push_str(part);
    } else {
        text.push((style, part.to_owned()));
    }
}

struct Word {
    start: usize,
    end: usize,
    value: String,
}

struct Words<'a> {
    chars: Peekable<CharIndices<'a>>,
    len: usize,
}

impl<'a> Words<'a> {
    fn new(line: &'a str) -> Self {
        Self {
            chars: line.char_indices().peekable(),
            len: line.len(),
        }
    }
}

impl Iterator for Words<'_> {
    type Item = Word;

    fn next(&mut self) -> Option<Word> {
        loop {
            match self.chars.peek().copied()? {
                (_, ' ' | '\t' | '\n') => {
                    self.chars.next();
                }
                (_, '#') => {
                    for (_, c) in self.chars.by_ref() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }

        let start = self.chars.peek()?.0;
        let mut value = String::new();
        let mut quote = None;
        while let Some((_, c)) = self.chars.peek().copied() {
            if quote.is_none() && matches!(c, ' ' | '\t' | '\n') {
                break;
            }
            self.chars.next();
            match (quote, c) {
                (Some('\''), '\'') | (Some('"'), '"') => quote = None,
                (None, '\'' | '"') => quote = Some(c),
                (Some('\''), _) => value.push(c),
                (_, '\\') => {
                    let Some((_, next)) = self.chars.next() else {
                        break;
                    };
                    if quote == Some('"') && !matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                        value.push('\\');
                    }
                    if next != '\n' {
                        value.push(next);
                    }
                }
                _ => value.push(c),
            }
        }
        let end = self.chars.peek().map_or(self.len, |(index, _)| *index);
        Some(Word { start, end, value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::Highlighter as _;

    fn mock_highlighter() -> Highlighter {
        Highlighter::new(Arc::new(Context::mock()))
    }

    #[test]
    fn test_highlighting() {
        let d = Style::new();
        let g = d.fg(Color::Green);
        let u = d.underline();
        let r = d.fg(Color::Red);
        let y = d.fg(Color::Yellow);
        let cases: &[(&str, &[(Style, &str)])] = &[
            ("", &[]),
            (" \t\n", &[(d, " \t\n")]),
            ("workers", &[(g, "workers")]),
            ("secrets", &[(g, "secrets")]),
            ("help", &[(g, "help")]),
            ("--help", &[(y, "--help")]),
            ("-h", &[(y, "-h")]),
            ("--foo=bar", &[(y, "--foo"), (d, "=bar")]),
            ("-- --help", &[(y, "--"), (d, " --help")]),
            ("s", &[(u, "s")]),
            ("wor", &[(u, "wor")]),
            ("wat", &[(r, "wat")]),
            ("é", &[(r, "é")]),
            (
                " \tlist  --owner bob -h",
                &[
                    (d, " \t"),
                    (g, "list"),
                    (d, "  "),
                    (y, "--owner"),
                    (d, " bob "),
                    (y, "-h"),
                ],
            ),
            (
                "list --owner=Zoë",
                &[(g, "list"), (d, " "), (y, "--owner"), (d, "=Zoë")],
            ),
            (
                "list --owner=\"Alice Smith\"",
                &[
                    (g, "list"),
                    (d, " "),
                    (y, "--owner"),
                    (d, "=\"Alice Smith\""),
                ],
            ),
            (
                "'list' 'some --text' \"--owner\"",
                &[(g, "'list'"), (d, " 'some --text' "), (y, "\"--owner\"")],
            ),
            (
                "li'st' foo\\ --text",
                &[(g, "li'st'"), (d, " foo\\ --text")],
            ),
            ("\"sta", &[(u, "\"sta")]),
            (
                "list 'unfinished --owner",
                &[(g, "list"), (d, " 'unfinished --owner")],
            ),
            ("list \\", &[(g, "list"), (d, " \\")]),
            (
                "list --owner=\"unfinished",
                &[(g, "list"), (d, " "), (y, "--owner"), (d, "=\"unfinished")],
            ),
            (
                "list --owner\\=bob",
                &[(g, "list"), (d, " "), (y, "--owner\\"), (d, "=bob")],
            ),
            (
                "# --owner\nlist # --help\n--owner",
                &[
                    (d, "# --owner\n"),
                    (g, "list"),
                    (d, " # --help\n"),
                    (y, "--owner"),
                ],
            ),
            (
                "list foo#bar -- --owner",
                &[(g, "list"), (d, " foo#bar "), (y, "--"), (d, " --owner")],
            ),
            (
                "list '--' --owner",
                &[(g, "list"), (d, " "), (y, "'--'"), (d, " --owner")],
            ),
            (
                "list '-' --x=one --y=two",
                &[
                    (g, "list"),
                    (d, " '-' "),
                    (y, "--x"),
                    (d, "=one "),
                    (y, "--y"),
                    (d, "=two"),
                ],
            ),
        ];
        let highlighter = mock_highlighter();
        for (line, expected) in cases {
            let actual = highlighter.highlight(line, line.len());
            let expected: Vec<_> = expected
                .iter()
                .map(|(style, text)| (*style, (*text).to_owned()))
                .collect();
            assert_eq!(actual.buffer, expected, "{line:?}");
            assert_eq!(actual.raw_string(), *line, "{line:?}");
        }
    }

    #[test]
    fn test_grammar_commands_and_word_decoding() {
        let highlighter = mock_highlighter();
        for name in &highlighter.commands {
            let actual = highlighter.highlight(name, 0);
            assert_eq!(
                actual.buffer,
                vec![(Style::new().fg(Color::Green), name.clone())]
            );
        }
        for line in [
            "# comment\n list --owner=alice",
            "start 'some --text' foo\\ bar \"a\\q\\\"b\"",
            "li'st' '' --owner='Élan'",
            "list foo#bar \\\n--help",
            "list '--' --owner",
        ] {
            let words: Vec<_> = Words::new(line).map(|word| word.value).collect();
            assert_eq!(Some(words), shlex::split(line), "{line:?}");
        }
    }

    #[test]
    fn test_parser_preservation() {
        let ctx = Arc::new(Context::mock());
        let highlighter = Highlighter::new(Arc::clone(&ctx));
        for line in [
            "start greet run-1 'Alice Smith' 5 --owner bob",
            "list --owner=alice # comment\n",
            "status 'é --text'",
            "list -- --owner",
            "'sta",
            "list \\",
        ] {
            let raw = highlighter.highlight(line, 0).raw_string();
            assert_eq!(raw, line);
            assert_eq!(shlex::split(&raw), shlex::split(line));
            assert_eq!(ctx.parse(&raw).is_ok(), ctx.parse(line).is_ok());
        }
    }

    #[test]
    fn test_partial_command_matching_at_cursor() {
        let highlighter = mock_highlighter();
        let incomplete = Style::new().underline();
        let invalid = Style::new().fg(Color::Red);
        assert_eq!(
            highlighter.highlight("stus", 2).buffer,
            vec![(incomplete, "stus".into())]
        );
        assert_eq!(
            highlighter.highlight("stus", 4).buffer,
            vec![(invalid, "stus".into())]
        );
    }
}
