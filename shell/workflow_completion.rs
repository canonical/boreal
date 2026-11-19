use boreal_shell::completion::Grammar;

use crate::command::Context;

impl Grammar for Context {
    fn cli(&self, previous: &[String]) -> clap::Command {
        self.cli(find_owner_hint(previous).as_deref())
    }

    fn typed_arg_candidates(&self, previous: &[String]) -> Vec<(String, Option<String>)> {
        if previous.first().map(String::as_str) != Some("run") {
            return Vec::new();
        }
        let Some((_, prefix)) = previous
            .split_last()
            .filter(|(flag, _)| *flag == "--workflow")
        else {
            return Vec::new();
        };
        let mut args = prefix.iter().skip(1).map(String::as_str);
        let mut worker = None;
        let mut owner = self.caller();
        while let Some(word) = args.next() {
            let (name, value) = word
                .split_once('=')
                .map_or((word, None), |(n, v)| (n, Some(v)));
            match name {
                "--owner" => {
                    let Some(value) = value.or_else(|| args.next()) else {
                        return Vec::new();
                    };
                    owner = value;
                }
                "--input-json" | "--id" if value.or_else(|| args.next()).is_some() => {}
                "--no-ask" => {}
                _ if !word.starts_with('-') && worker.is_none() => worker = Some(word),
                _ => return Vec::new(),
            }
        }
        let Some(worker) = worker else {
            return Vec::new();
        };
        let Ok(pkg) = self.run_package(owner, worker) else {
            return Vec::new();
        };
        pkg.workflows()
            .map(|(alias, workflow)| {
                (
                    alias.to_owned(),
                    (!workflow.description().is_empty()).then(|| workflow.description().to_owned()),
                )
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use boreal_common::catalog::Catalog;
    use boreal_shell::completion::Completer;
    use reedline::{Completer as _, Suggestion};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn mock_completer() -> Completer<Context> {
        Completer::new(Arc::new(Context::mock()))
    }

    fn candidate(
        comp: &mut Completer<Context>,
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
            ("v", "view", "view "),
            ("view job-1 --owner b", "bob", "view job-1 --owner bob "),
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
                "run greet --id job --owner b",
                "bob",
                "run greet --id job --owner bob ",
            ),
            ("permissions b", "bob", "permissions bob "),
            ("secrets ", "set", "secrets set "),
        ];
        for (line, display, expected) in cases {
            let suggestion = candidate(&mut comp, line, line.len(), display)
                .unwrap_or_else(|| panic!("missing {display:?} for {line:?}"));
            assert_eq!(apply(line, &suggestion), expected, "{line:?}");
        }

        assert!(candidate(&mut comp, "up", 2, "updater").is_none());

        // When --owner bob is specified first, `count` must NOT be completed for `alice`
        // because `alice` only has write permission on `bob/greet`!
        let bob_workers: Vec<_> = comp
            .complete("run --owner bob ", 16)
            .into_iter()
            .map(|s| s.display_value().to_owned())
            .collect();
        assert!(bob_workers.iter().any(|s| s == "greet"));
        assert!(!bob_workers.iter().any(|s| s == "count"));

        // Workflow aliases still come from the selected, permitted worker.
        let workers_yaml = "\
train-pkg:
  workflow: TrainWf
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
        let alias = "run train --workflow train";
        assert!(candidate(&mut custom_comp, alias, alias.len(), "train-pkg").is_some());
        let no_positionals = "run train f";
        assert!(
            candidate(
                &mut custom_comp,
                no_positionals,
                no_positionals.len(),
                "fast"
            )
            .is_none()
        );
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
                "run greet --id 'Zoë' --o",
                "run greet --id 'Zoë' --o".len(),
                "--owner",
                "run greet --id 'Zoë' --owner ",
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
