use std::ffi::OsString;

use reedline::{Span, Suggestion};

/// Completes the local command grammar without executing commands.
///
/// The Reedline menu must use `InputMode::FullBuffer` and `SuggestedSpan`;
/// cursor-prefix input cannot protect a suffix the completer cannot see.
#[derive(Default)]
pub struct Completer;

impl reedline::Completer for Completer {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let Some(word) = word_at_cursor(line, pos) else {
            return Vec::new();
        };
        let Some(previous) = shlex::split(&line[..word.start]) else {
            return Vec::new();
        };

        let mut command = crate::command::cli();
        let mut args = Vec::<OsString>::new();
        if !command.is_no_binary_name_set() {
            args.push(command.get_name().into());
        }
        args.extend(previous.into_iter().map(OsString::from));
        let arg_index = args.len();
        args.push(word.prefix.clone().into());

        let Ok(candidates) = clap_complete::engine::complete(&mut command, args, arg_index, None)
        else {
            return Vec::new();
        };
        candidates
            .into_iter()
            .filter_map(|candidate| {
                let value = candidate.get_value().to_str()?;
                if !value.starts_with(&word.prefix) || !value.ends_with(&word.suffix) {
                    return None;
                }
                let end = value.len().checked_sub(word.suffix.len())?;
                let insertion = value.get(word.prefix.len()..end)?;

                // All grammar candidates are simple ASCII words and flags. Reject
                // other candidates rather than insert unsafe or misquoted text.
                if !insertion
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'='))
                {
                    return None;
                }
                let mut replacement = line[word.start..pos].to_owned();
                replacement.push_str(insertion);
                // An unfinished quote at the end of the buffer needs a closing
                // delimiter. If there is a suffix, it remains byte-for-byte intact.
                if word.end == pos
                    && let Some(quote) = word.quote_at_cursor
                {
                    replacement.push(quote);
                }
                Some(Suggestion {
                    value: replacement,
                    display_override: Some(value.to_owned()),
                    description: candidate.get_help().map(ToString::to_string),
                    span: Span::new(word.start, pos),
                    append_whitespace: word.end == pos && pos == line.len(),
                    ..Suggestion::default()
                })
            })
            .collect()
    }
}

struct Word {
    start: usize,
    end: usize,
    prefix: String,
    suffix: String,
    quote_at_cursor: Option<char>,
}

fn is_separator(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n')
}

// Find raw byte boundaries while observing shlex's quotes and escapes. Shlex
// itself decodes the words; appending a closing quote permits partial input.
fn word_at_cursor(line: &str, pos: usize) -> Option<Word> {
    if pos > line.len() || !line.is_char_boundary(pos) || line.contains('\0') {
        return None;
    }
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line[..pos].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote != Some('\'') && c == '\\' {
            start.get_or_insert(i);
            escaped = true;
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            start.get_or_insert(i);
            quote = Some(c);
        } else if is_separator(c) {
            start = None;
        } else {
            // Comments have special shlex semantics, so fail closed rather
            // than mistake their contents for command arguments.
            if start.is_none() && c == '#' {
                return None;
            }
            start.get_or_insert(i);
        }
    }
    if escaped {
        return None;
    }
    let start = start.unwrap_or(pos);
    let quote_at_cursor = quote;
    let prefix = decode_word(&line[start..pos], quote_at_cursor)?;

    let mut end = line.len();
    let mut escaped = false;
    for (offset, c) in line[pos..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote != Some('\'') && c == '\\' {
            escaped = true;
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if is_separator(c) {
            end = pos + offset;
            break;
        } else if start == pos && offset == 0 && c == '#' {
            return None;
        }
    }
    if escaped || (quote.is_some() && end != pos) {
        return None;
    }
    let full = decode_word(&line[start..end], quote)?;
    let suffix = full.strip_prefix(&prefix)?.to_owned();
    Some(Word {
        start,
        end,
        prefix,
        suffix,
        quote_at_cursor,
    })
}

fn decode_word(raw: &str, open_quote: Option<char>) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }
    let mut closed = raw.to_owned();
    if let Some(quote) = open_quote {
        closed.push(quote);
    }
    let mut words = shlex::split(&closed)?;
    if words.len() == 1 { words.pop() } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::Completer as _;

    fn candidate(line: &str, pos: usize, display: &str) -> Option<Suggestion> {
        Completer
            .complete(line, pos)
            .into_iter()
            .find(|suggestion| suggestion.display_value() == display)
    }

    fn apply(line: &str, suggestion: &Suggestion) -> String {
        let mut result = line.to_owned();
        let mut value = suggestion.value.clone();
        if suggestion.append_whitespace {
            value.push(' ');
        }
        result.replace_range(suggestion.span.start..suggestion.span.end, &value);
        result
    }

    #[test]
    fn test_completes_commands_values_and_flags() {
        let cases = [
            ("", "workers", "workers "),
            ("", "start", "start "),
            ("", "help", "help "),
            ("sta", "start", "start "),
            ("per", "permissions", "permissions "),
            ("start ", "greet", "start greet "),
            ("start c", "count", "start count "),
            ("list --o", "--owner", "list --owner "),
            ("list --owner ", "alice", "list --owner alice "),
            ("list --owner=", "--owner=bob", "list --owner=bob "),
            ("status run --owner b", "bob", "status run --owner bob "),
            (
                "result run --owner=b",
                "--owner=bob",
                "result run --owner=bob ",
            ),
            ("cancel run --owner a", "alice", "cancel run --owner alice "),
            (
                "start greet id name --owner b",
                "bob",
                "start greet id name --owner bob ",
            ),
            ("permissions b", "bob", "permissions bob "),
        ];
        for (line, display, expected) in cases {
            let suggestion = candidate(line, line.len(), display)
                .unwrap_or_else(|| panic!("missing {display:?} for {line:?}"));
            assert_eq!(apply(line, &suggestion), expected, "{line:?}");
        }
        for line in ["workers ", "exit ", "permissions alice "] {
            let displays: Vec<_> = Completer
                .complete(line, line.len())
                .into_iter()
                .map(|s| s.display_value().to_owned())
                .collect();
            assert!(
                !displays.iter().any(|s| s == "workers" || s == "alice"),
                "{line:?}"
            );
        }
    }

    #[test]
    fn test_quotes_and_cursor_suffixes_are_preserved() {
        let cases = [
            ("'sta", 4, "start", "'start' "),
            ("\"sta", 4, "start", "\"start\" "),
            (
                "list --owner=\"a",
                15,
                "--owner=alice",
                "list --owner=\"alice\" ",
            ),
            ("stus --owner bob", 2, "status", "status --owner bob"),
            (
                "list --owner=ae trailing",
                14,
                "--owner=alice",
                "list --owner=alice trailing",
            ),
            (
                "list --owner 'ae' trailing",
                15,
                "alice",
                "list --owner 'alice' trailing",
            ),
            (
                "start greet id 'Zoë' --o",
                25,
                "--owner",
                "start greet id 'Zoë' --owner ",
            ),
        ];
        for (line, pos, display, expected) in cases {
            let suggestion = candidate(line, pos, display)
                .unwrap_or_else(|| panic!("missing {display:?} for {line:?} at {pos}"));
            assert_eq!(apply(line, &suggestion), expected, "{line:?} at {pos}");
            assert_eq!(
                &apply(line, &suggestion)[apply(line, &suggestion).len() - (line.len() - pos)..],
                &line[pos..]
            );
        }
    }

    #[test]
    fn test_unsupported_or_incompatible_input_returns_no_candidates() {
        for (line, pos) in [
            ("stXYZ", 2),
            ("start \\", 7),
            ("start # comment", 15),
            ("list --owner 'ab", 14),
            ("é", 1),
            ("start", 6),
            ("list\0", 5),
        ] {
            assert!(
                Completer.complete(line, pos).is_empty(),
                "{line:?} at {pos}"
            );
        }
    }
}
