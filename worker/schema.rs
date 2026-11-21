use boreal_schema::{self as catalog, Input};
use schemars::JsonSchema;
use temporalio_common::WorkflowDefinition;
use temporalio_workflow::workflows::WorkflowImplementation;

pub struct WorkerSchema(catalog::Worker);

pub struct WorkflowSchema(catalog::Workflow);

impl WorkerSchema {
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

    pub fn from_exports(name: &'static str, exports: Vec<WorkflowSchema>) -> Self {
        Self(catalog::Worker::named(
            name,
            exports.into_iter().map(|export| export.0).collect(),
        ))
    }
}

impl WorkflowSchema {
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

pub fn emit_schema(workers: &[WorkerSchema]) -> Result<(), String> {
    print!("{}", render_schema(workers)?);
    Ok(())
}

pub fn render_schema(workers: &[WorkerSchema]) -> Result<String, String> {
    catalog::render(workers.iter().map(|worker| &worker.0))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        #[run(name = "Count.V1")]
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

    crate::declare_worker! {
        package = "example",
        workflows = {
            export greet: Greeting => "Say hello",
            internal self::Child,
            export count: Counting => "Count objects",
        },
        configure = |opts| opts.graceful_shutdown_period(crate::DEFAULT_SHUTDOWN_GRACE),
    }

    #[test]
    fn test_declaration_registers_all_and_emits_only_exports() {
        let mut options = worker_options("alice-tools".into()).unwrap();
        assert_eq!(options.task_queue, "alice-tools");
        assert!(options.register_workflow::<Greeting>().is_err());
        assert!(options.register_workflow::<Counting>().is_err());
        assert!(options.register_workflow::<Child>().is_err());

        let yaml = render_schema(&[schema()]).unwrap();
        assert!(yaml.contains("example:\n  workflows:\n    greet:\n      workflow: \"Greeting\""));
        assert!(yaml.contains("    count:\n      workflow: \"Count.V1\""));
        assert!(!yaml.contains("Child"));
    }

    #[test]
    fn test_entrypoint_rejects_an_unrecognized_binary() {
        let err = crate::run_entrypoint(
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
