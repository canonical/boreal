use std::ffi::OsString;
use std::sync::Arc;

use boreal_common::catalog::ArgKind;
use reedline::{Span, Suggestion};

use crate::command::Context;

/// Completes the dynamic catalog-driven grammar without executing commands.
///
/// The Reedline menu must use `InputMode::FullBuffer` and `SuggestedSpan`;
/// cursor-prefix input cannot protect a suffix the completer cannot see.
#[derive(Clone)]
pub struct Completer {
    ctx: Arc<Context>,
}

impl Completer {
    pub fn new(ctx: Arc<Context>) -> Self {
        Self { ctx }
    }

    fn typed_arg_candidates(&self, previous: &[String]) -> Vec<(String, Option<String>)> {
        if previous.first().map(String::as_str) != Some("run") {
            return Vec::new();
        }
        let mut owner = self.ctx.caller().to_owned();
        let mut positionals = Vec::new();
        let mut idx = 1usize;
        while idx < previous.len() {
            let tok = &previous[idx];
            if tok == "--owner" {
                if let Some(val) = previous.get(idx + 1) {
                    owner.clone_from(val);
                    idx += 2;
                    continue;
                }
                return Vec::new();
            }
            if let Some(val) = tok.strip_prefix("--owner=") {
                owner = val.to_owned();
                idx += 1;
                continue;
            }
            if tok.starts_with('-') {
                idx += 1;
                continue;
            }
            positionals.push(tok.as_str());
            idx += 1;
        }
        // positionals[0] = worker, positionals[1] = workflow_id, positionals[2..] = schema args
        if positionals.len() < 2 {
            return Vec::new();
        }
        let worker = positionals[0];
        let arg_index = positionals.len() - 2;
        let Ok((w, pkg)) = self.ctx.catalog().worker_and_package(&owner, worker) else {
            return Vec::new();
        };
        if !self
            .ctx
            .catalog()
            .can_write(self.ctx.caller(), &owner, w.name())
        {
            return Vec::new();
        }
        let Some(spec) = pkg.args().get(arg_index) else {
            return Vec::new();
        };
        let desc = if spec.description().is_empty() {
            None
        } else {
            Some(spec.description().to_owned())
        };
        match spec.kind() {
            ArgKind::Enum { choices } => {
                choices.iter().map(|c| (c.clone(), desc.clone())).collect()
            }
            ArgKind::Bool => vec![("true".into(), desc.clone()), ("false".into(), desc)],
            _ => Vec::new(),
        }
    }
}

fn find_owner_hint(words: &[String]) -> Option<String> {
    let mut i = 0;
    while i < words.len() {
        if words[i] == "--owner"
            && let Some(val) = words.get(i + 1)
        {
            return Some(val.clone());
        }
        if let Some(val) = words[i].strip_prefix("--owner=") {
            return Some(val.to_owned());
        }
        i += 1;
    }
    None
}

impl reedline::Completer for Completer {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let Some(word) = word_at_cursor(line, pos) else {
            return Vec::new();
        };
        let Some(previous) = shlex::split(&line[..word.start]) else {
            return Vec::new();
        };

        let owner_hint = find_owner_hint(&previous);
        let mut command = self.ctx.cli(owner_hint.as_deref());
        let mut args = Vec::<OsString>::new();
        if !command.is_no_binary_name_set() {
            args.push(command.get_name().into());
        }
        args.extend(previous.iter().map(OsString::from));
        let arg_index = args.len();
        args.push(word.prefix.clone().into());

        let mut raw_candidates: Vec<(String, Option<String>)> = Vec::new();
        if !word.prefix.starts_with('-') {
            raw_candidates.extend(self.typed_arg_candidates(&previous));
        }
        if let Ok(clap_items) = clap_complete::engine::complete(&mut command, args, arg_index, None)
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use boreal_common::catalog::Catalog;
    use reedline::Completer as _;
    use std::collections::BTreeMap;

    fn mock_completer() -> Completer {
        Completer::new(Arc::new(Context::mock()))
    }

    fn candidate(
        comp: &mut Completer,
        line: &str,
        pos: usize,
        display: &str,
    ) -> Option<Suggestion> {
        comp.complete(line, pos)
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
    fn test_completes_commands_values_flags_and_filters_by_permission() {
        let mut comp = mock_completer();
        let cases = [
            ("", "workers", "workers "),
            ("", "run", "run "),
            ("", "secrets", "secrets "),
            ("", "help", "help "),
            ("ru", "run", "run "),
            ("st", "status", "status "),
            ("per", "permissions", "permissions "),
            ("run ", "greet", "run greet "),
            ("run c", "count", "run count "),
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
                "run greet id name --owner b",
                "bob",
                "run greet id name --owner bob ",
            ),
            ("permissions b", "bob", "permissions bob "),
            ("secrets ", "set", "secrets set "),
        ];
        for (line, display, expected) in cases {
            let suggestion = candidate(&mut comp, line, line.len(), display)
                .unwrap_or_else(|| panic!("missing {display:?} for {line:?}"));
            assert_eq!(apply(line, &suggestion), expected, "{line:?}");
        }

        // When --owner bob is specified first, `count` must NOT be completed for `alice`
        // because `alice` only has write permission on `bob/greet`!
        let bob_workers: Vec<_> = comp
            .complete("run --owner bob ", 16)
            .into_iter()
            .map(|s| s.display_value().to_owned())
            .collect();
        assert!(bob_workers.iter().any(|s| s == "greet"));
        assert!(!bob_workers.iter().any(|s| s == "count"));

        // Enum and bool argument completion from dynamic catalog!
        let workers_yaml = "\
train-pkg:
  workflowType: TrainWf
  args:
  - name: mode
    type: enum
    choices: [fast, accurate]
  - name: dry_run
    type: bool
    default: false
";
        let mut users_yaml = BTreeMap::new();
        users_yaml.insert(
            "alice".into(),
            "user: alice\npermissions: {read: false, write: false}\nworkers:\n  train:\n    package: train-pkg\n    permissions: {read: false, write: false}\n".into(),
        );
        users_yaml.insert(
            "bob".into(),
            "user: bob\npermissions: {read: false, write: false}\nworkers: {}\n".into(),
        );
        let custom_ctx = Arc::new(Context::new(
            Catalog::from_yaml(workers_yaml, &users_yaml).unwrap(),
            "alice",
        ));
        let mut custom_comp = Completer::new(custom_ctx);
        assert!(candidate(&mut custom_comp, "run train job1 f", 16, "fast").is_some());
        assert!(candidate(&mut custom_comp, "run train job1 fast t", 21, "true").is_some());
        // Alice has no access to bob -> bob must NOT appear in `list --owner ` or `permissions `!
        assert!(candidate(&mut custom_comp, "list --owner ", 13, "bob").is_none());
        assert!(candidate(&mut custom_comp, "permissions ", 12, "bob").is_none());
    }

    #[test]
    fn test_quotes_and_cursor_suffixes_are_preserved() {
        let mut comp = mock_completer();
        let cases = [
            ("'ru", 3, "run", "'run' "),
            ("\"ru", 3, "run", "\"run\" "),
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
                "run greet id 'Zoë' --o",
                23,
                "--owner",
                "run greet id 'Zoë' --owner ",
            ),
        ];
        for (line, pos, display, expected) in cases {
            let suggestion = candidate(&mut comp, line, pos, display)
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
        let mut comp = mock_completer();
        for (line, pos) in [
            ("stXYZ", 2),
            ("run \\", 5),
            ("run # comment", 13),
            ("list --owner 'ab", 14),
            ("é", 1),
            ("run", 4),
            ("list\0", 5),
        ] {
            assert!(comp.complete(line, pos).is_empty(), "{line:?} at {pos}");
        }
    }
}
