//! Declare the native Temporal workflows exposed through the Boreal shell.

use boreal_schema::{self as catalog, Input};
use schemars::JsonSchema;
use temporalio_common::WorkflowDefinition;
use temporalio_workflow::workflows::WorkflowImplementation;

/// A worker's public launch catalog.
pub struct Worker(catalog::Worker);

/// An exported native Temporal workflow and its schema.
pub struct Workflow(catalog::Workflow);

impl Worker {
    /// Declares a single-workflow catalog keyed by the worker binary name.
    pub fn new<T: JsonSchema>(
        name: &'static str,
        workflow: &'static str,
        description: &'static str,
    ) -> Self {
        Self(catalog::Worker::single(
            name,
            input::<T>(workflow, description),
        ))
    }

    pub fn from_exports(name: &'static str, exports: Vec<Workflow>) -> Self {
        Self(catalog::Worker::named(
            name,
            exports.into_iter().map(|export| export.0).collect(),
        ))
    }
}

impl Workflow {
    /// Export the native Temporal workflow type under its own name.
    pub fn of_type<W>(description: &'static str) -> Self
    where
        W: WorkflowImplementation,
        <W::Run as WorkflowDefinition>::Input: JsonSchema,
    {
        Self::of::<W>(W::name(), description)
    }

    pub fn of<W>(alias: &'static str, description: &'static str) -> Self
    where
        W: WorkflowImplementation,
        <W::Run as WorkflowDefinition>::Input: JsonSchema,
    {
        Self(catalog::Workflow::new(
            alias,
            input::<<W::Run as WorkflowDefinition>::Input>(W::name(), description),
        ))
    }
}

fn input<T: JsonSchema>(workflow: &'static str, description: &'static str) -> Input {
    Input::new(workflow, description, || schemars::schema_for!(T))
}

/// Writes the launch catalog to stdout for the package's `schema` binary.
pub fn emit(workers: &[Worker]) -> Result<(), String> {
    print!("{}", render(workers)?);
    Ok(())
}

fn render(workers: &[Worker]) -> Result<String, String> {
    catalog::render(workers.iter().map(|worker| &worker.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker;
    use temporalio_macros::{workflow, workflow_methods};
    use temporalio_sdk::{WorkflowContext, WorkflowResult};

    #[workflow]
    #[derive(Default)]
    struct Greeting;

    #[workflow_methods]
    impl Greeting {
        #[run]
        async fn run(_: &mut WorkflowContext<Self>, _input: String) -> WorkflowResult<()> {
            Ok(())
        }
    }

    #[workflow]
    #[derive(Default)]
    struct Counting;

    #[workflow_methods]
    impl Counting {
        #[run(name = "count-objects")]
        async fn run(_: &mut WorkflowContext<Self>, _input: u64) -> WorkflowResult<()> {
            Ok(())
        }
    }

    #[workflow]
    #[derive(Default)]
    struct Child;

    #[workflow_methods]
    impl Child {
        #[run]
        async fn run(_: &mut WorkflowContext<Self>) -> WorkflowResult<()> {
            Ok(())
        }
    }

    worker::main! {
        package = "example",
        workflows = {
            export say_hello: Greeting => "Say hello",
            internal self::Child,
            export self::Counting => "Count objects",
        },
        configure = |opts| opts.graceful_shutdown_period(worker::DEFAULT_SHUTDOWN_GRACE),
    }

    #[test]
    fn test_declaration_registers_all_and_emits_only_exports() {
        let mut options = worker_options("alice-tools".into()).unwrap();
        assert_eq!(options.task_queue, "alice-tools");
        assert!(options.register_workflow::<Greeting>().is_err());
        assert!(options.register_workflow::<Counting>().is_err());
        assert!(options.register_workflow::<Child>().is_err());

        let yaml = render(&[schema()]).unwrap();
        assert!(
            yaml.contains("example:\n  workflows:\n    say_hello:\n      workflow: \"Greeting\"")
        );
        assert!(yaml.contains("    count-objects:\n      workflow: \"count-objects\""));
        assert!(!yaml.contains("Child"));
    }

    #[test]
    fn test_entrypoint_rejects_an_unrecognized_binary() {
        let err = worker::__run_entrypoint(
            Some("other"),
            "example",
            |_| panic!("must not configure a worker"),
            || panic!("must not generate a schema"),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("expected Cargo binary 'example' or 'schema'")
        );
    }
}
