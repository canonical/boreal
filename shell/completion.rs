use std::ffi::OsString;
use std::sync::Arc;

use reedline::{Span, Suggestion};

pub trait Grammar {
    fn cli(&self, previous: &[String]) -> clap::Command;

    fn typed_arg_candidates(&self, _previous: &[String]) -> Vec<(String, Option<String>)> {
        Vec::new()
    }
}

/// Completes a role's grammar without executing commands. Use with a menu
/// configured for `FullBuffer` and `SuggestedSpan` to preserve the cursor suffix.
pub struct Completer<G> {
    grammar: Arc<G>,
}

impl<G> Completer<G> {
    pub fn new(grammar: Arc<G>) -> Self {
        Self { grammar }
    }
}

impl<G: Grammar + Send + Sync> reedline::Completer for Completer<G> {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        candidates(self.grammar.as_ref(), line, pos)
    }
}

pub fn candidates(grammar: &impl Grammar, line: &str, pos: usize) -> Vec<Suggestion> {
    let Some(word) = word_at_cursor(line, pos) else {
        return Vec::new();
    };
    let Some(previous) = shlex::split(&line[..word.start]) else {
        return Vec::new();
    };

    let mut command = grammar.cli(&previous);
    let mut args = Vec::<OsString>::new();
    if !command.is_no_binary_name_set() {
        args.push(command.get_name().into());
    }
    args.extend(previous.iter().map(OsString::from));
    let arg_index = args.len();
    args.push(word.prefix.clone().into());

    let mut raw_candidates = if word.prefix.starts_with('-') {
        Vec::new()
    } else {
        grammar.typed_arg_candidates(&previous)
    };
    if let Ok(clap_items) = clap_complete::engine::complete(&mut command, args, arg_index, None) {
        for item in clap_items {
            if let Some(val) = item.get_value().to_str()
                && !raw_candidates.iter().any(|(existing, _)| existing == val)
            {
                raw_candidates.push((val.to_owned(), item.get_help().map(ToString::to_string)));
            }
        }
    }

    raw_candidates
        .into_iter()
        .filter_map(|(value, description)| {
            if !value.starts_with(&word.prefix) || !value.ends_with(&word.suffix) {
                return None;
            }
            let end = value.len().checked_sub(word.suffix.len())?;
            let insertion = value.get(word.prefix.len()..end)?;

            if !insertion
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'='))
            {
                return None;
            }
            let mut replacement = line[word.start..pos].to_owned();
            replacement.push_str(insertion);
            if word.end == pos
                && let Some(quote) = word.quote_at_cursor
            {
                replacement.push(quote);
            }
            Some(Suggestion {
                value: replacement,
                display_override: Some(value),
                description,
                span: Span::new(word.start, pos),
                append_whitespace: word.end == pos && pos == line.len(),
                ..Suggestion::default()
            })
        })
        .collect()
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
