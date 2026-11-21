use std::time::Duration;

use boreal_sdk::worker;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use temporalio_macros::{activities, activity_definitions, workflow, workflow_methods};
use temporalio_sdk::{
    ActivityExecutionError, ActivityOptions, ApplicationFailure, ChildWorkflowOptions, TimerResult,
    WorkerOptions, WorkflowContext, WorkflowContextView, WorkflowResult, WorkflowTermination,
    activities::{ActivityContext, ActivityError},
};
use validator::Validate;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Validate)]
pub struct DemoInput {
    /// Recipient name
    #[validate(length(min = 1))]
    pub name: String,
    /// Timer duration in seconds
    #[validate(range(min = 1, max = 7200))]
    pub wait_seconds: u64,
}

fn reject_invalid(input: &DemoInput) -> Result<(), WorkflowTermination> {
    input.validate().map_err(|err| {
        WorkflowTermination::failed_application(ApplicationFailure::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            err.to_string(),
        )))
    })
}

pub struct DemoActivities;

#[activities]
impl DemoActivities {
    #[activity]
    pub async fn greet(_ctx: ActivityContext, name: String) -> Result<String, ActivityError> {
        Ok(format!("Hello, {name}!"))
    }
}

pub struct Vault;

#[activity_definitions]
impl Vault {
    #[activity(name = "verify-fixture-token")]
    pub fn verify_fixture_token() -> Result<(), ActivityError> {
        unimplemented!()
    }
}

pub use vault::VerifyFixtureToken;

pub struct CountActivities;

#[activities]
impl CountActivities {
    #[activity]
    pub async fn count_chars(_ctx: ActivityContext, name: String) -> Result<String, ActivityError> {
        Ok(format!("Character count: {}", name.chars().count()))
    }
}

#[workflow]
pub struct ChildGreetingWorkflow {
    input: DemoInput,
}

#[workflow_methods]
impl ChildGreetingWorkflow {
    #[init]
    fn new(_ctx: &WorkflowContextView, input: DemoInput) -> Self {
        Self { input }
    }

    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<String> {
        let input = ctx.state(|s| s.input.clone());
        reject_invalid(&input)?;
        if ctx.timer(Duration::from_secs(input.wait_seconds)).await == TimerResult::Cancelled {
            return Err(WorkflowTermination::cancelled());
        }
        ctx.execute_activity(
            DemoActivities::greet,
            input.name,
            ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
        )
        .await
        .map_err(|err| match err {
            ActivityExecutionError::Cancelled(_) => WorkflowTermination::cancelled(),
            other => other.into(),
        })
    }
}

#[workflow]
pub struct DemoWorkflow {
    input: DemoInput,
}

impl DemoWorkflow {
    /// Marker value for `ScheduleAction::start_workflow`. The payload is the
    /// second argument; this instance only supplies the workflow type name.
    #[allow(dead_code)]
    pub fn scheduled(input: DemoInput) -> Self {
        Self { input }
    }
}

#[workflow_methods]
impl DemoWorkflow {
    #[init]
    fn new(_ctx: &WorkflowContextView, input: DemoInput) -> Self {
        Self { input }
    }

    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<String> {
        let input = ctx.state(|s| s.input.clone());
        reject_invalid(&input)?;
        if let Some(child_target) = input.name.strip_prefix("child:") {
            let child_id = format!("{}-child", ctx.workflow_id());
            let started = ctx
                .start_child_workflow(
                    ChildGreetingWorkflow::run,
                    DemoInput {
                        name: child_target.to_owned(),
                        wait_seconds: input.wait_seconds,
                    },
                    ChildWorkflowOptions::workflow_id(child_id),
                )
                .await
                .map_err(WorkflowTermination::from)?;
            return started.result().await.map_err(WorkflowTermination::from);
        }
        if ctx.timer(Duration::from_secs(input.wait_seconds)).await == TimerResult::Cancelled {
            return Err(WorkflowTermination::cancelled());
        }
        if input.name == "vault-check" {
            let options = ActivityOptions::with_schedule_to_close_timeout(Duration::from_secs(8))
                .task_queue(worker::vault_task_queue(ctx.namespace()))
                .do_not_eagerly_execute(true)
                .build();
            ctx.execute_activity(Vault::verify_fixture_token, (), options)
                .await
                .map_err(WorkflowTermination::from)?;
            return Ok("Central vault verified the token.".into());
        }
        ctx.execute_activity(
            DemoActivities::greet,
            input.name,
            ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
        )
        .await
        .map_err(|err| match err {
            ActivityExecutionError::Cancelled(_) => WorkflowTermination::cancelled(),
            other => other.into(),
        })
    }
}

#[workflow]
pub struct CountWorkflow {
    input: DemoInput,
}

#[workflow_methods]
impl CountWorkflow {
    #[init]
    fn new(_ctx: &WorkflowContextView, input: DemoInput) -> Self {
        Self { input }
    }

    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<String> {
        let input = ctx.state(|s| s.input.clone());
        reject_invalid(&input)?;
        if ctx.timer(Duration::from_secs(input.wait_seconds)).await == TimerResult::Cancelled {
            return Err(WorkflowTermination::cancelled());
        }
        ctx.execute_activity(
            CountActivities::count_chars,
            input.name,
            ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
        )
        .await
        .map_err(|err| match err {
            ActivityExecutionError::Cancelled(_) => WorkflowTermination::cancelled(),
            other => other.into(),
        })
    }
}

pub fn greet_options(queue: String) -> worker::Result<WorkerOptions> {
    Ok(WorkerOptions::new(queue)
        .graceful_shutdown_period(worker::DEFAULT_SHUTDOWN_GRACE)
        .register_activities(DemoActivities)
        .register_workflow::<ChildGreetingWorkflow>()?
        .register_workflow::<DemoWorkflow>()?
        .build())
}

pub fn count_options(queue: String) -> worker::Result<WorkerOptions> {
    Ok(WorkerOptions::new(queue)
        .graceful_shutdown_period(worker::DEFAULT_SHUTDOWN_GRACE)
        .register_activities(CountActivities)
        .register_workflow::<CountWorkflow>()?
        .build())
}
