use boreal_common::catalog;
use boreal_shell::completion::Grammar;

use crate::command::{self, Context};

impl Grammar for Context {
    fn cli(&self, _previous: &[String]) -> clap::Command {
        self.cli()
    }

    fn typed_arg_candidates(
        &self,
        previous: &[String],
        prefix: &str,
    ) -> Vec<(String, Option<String>)> {
        let first = previous
            .iter()
            .position(|word| word != "--no-ask")
            .unwrap_or(previous.len());
        let Some((verb, args)) = previous[first..].split_first() else {
            return Vec::new();
        };
        match verb.as_str() {
            "status" | "result" | "view" | "cancel"
                if args.iter().all(|word| word == "--no-ask") =>
            {
                return prefix
                    .rsplit_once('/')
                    .filter(|(id, _)| catalog::validate_workflow_id(id).is_ok())
                    .into_iter()
                    .flat_map(|(id, _)| {
                        self.owners(verb == "cancel")
                            .into_iter()
                            .map(move |owner| (format!("{id}/{owner}"), None))
                    })
                    .collect();
            }
            "run" => {}
            _ => return Vec::new(),
        }
        let Some((_, args)) = args.split_last().filter(|(flag, _)| *flag == "--workflow") else {
            return Vec::new();
        };
        let mut args = args.iter().map(String::as_str);
        let mut selector = None;
        while let Some(word) = args.next() {
            let (name, value) = word
                .split_once('=')
                .map_or((word, None), |(n, v)| (n, Some(v)));
            match name {
                "--input-json" | "--id" if value.or_else(|| args.next()).is_some() => {}
                "--no-ask" => {}
                _ if !word.starts_with('-') && selector.is_none() => selector = Some(word),
                _ => return Vec::new(),
            }
        }
        let Some((worker, owner)) = selector.and_then(|value| command::split_selector(value).ok())
        else {
            return Vec::new();
        };
        let Ok(pkg) = self.run_package(owner.unwrap_or(self.caller()), worker) else {
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
            ("view job-1/b", "job-1/bob", "view job-1/bob "),
            ("per", "permissions", "permissions "),
            ("run ", "greet", "run greet "),
            ("run c", "count", "run count "),
            ("list ", "alice", "list alice "),
            ("list b", "bob", "list bob "),
            ("status run/b", "run/bob", "status run/bob "),
            (
                "result suite/run/b",
                "suite/run/bob",
                "result suite/run/bob ",
            ),
            ("cancel run/a", "run/alice", "cancel run/alice "),
            (
                "run --id job greet/b",
                "greet/bob",
                "run --id job greet/bob ",
            ),
            ("run greet", "greet/bob", "run greet/bob "),
            ("permissions b", "bob", "permissions bob "),
            ("secrets ", "set", "secrets set "),
        ];
        for (line, display, expected) in cases {
            let suggestion = candidate(&mut comp, line, line.len(), display)
                .unwrap_or_else(|| panic!("missing {display:?} for {line:?}"));
            assert_eq!(apply(line, &suggestion), expected, "{line:?}");
        }

        assert!(candidate(&mut comp, "up", 2, "updater").is_none());

        assert!(candidate(&mut comp, "run count/b", 11, "count/bob").is_none());

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
        let alias = "run train/alice --workflow train";
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
        assert!(candidate(&mut custom_comp, "list ", 5, "bob").is_none());
        assert!(candidate(&mut custom_comp, "status id/", 10, "id/bob").is_none());
        assert!(candidate(&mut custom_comp, "permissions ", 12, "bob").is_none());
    }

    #[test]
    fn test_quotes_and_cursor_suffixes_are_preserved() {
        let mut comp = mock_completer();
        let cases = [
            ("'ru", 3, "run", "'run' "),
            ("\"ru", 3, "run", "\"run\" "),
            ("list \"a", 7, "alice", "list \"alice\" "),
            ("stus id/bob", 2, "status", "status id/bob"),
            ("list ae trailing", 6, "alice", "list alice trailing"),
            ("list 'ae' trailing", 7, "alice", "list 'alice' trailing"),
            (
                "run --id 'Zoë' 'greet/b",
                "run --id 'Zoë' 'greet/b".len(),
                "greet/bob",
                "run --id 'Zoë' 'greet/bob' ",
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
            ("list 'ab", 7),
            ("é", 1),
            ("run", 4),
            ("list\0", 5),
        ] {
            assert!(comp.complete(line, pos).is_empty(), "{line:?} at {pos}");
        }
    }
}
