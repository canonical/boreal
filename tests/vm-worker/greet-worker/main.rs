use std::time::Duration;

use serde::{Deserialize, Serialize};
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    ActivityExecutionError, ActivityOptions, ChildWorkflowOptions, TimerResult, WorkerOptions,
    WorkflowContext, WorkflowContextView, WorkflowResult, WorkflowTermination,
    activities::{ActivityContext, ActivityError},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DemoInput {
    pub name: String,
    pub wait_seconds: u64,
}

pub struct DemoActivities;

#[activities]
impl DemoActivities {
    #[activity]
    pub async fn greet(_ctx: ActivityContext, name: String) -> Result<String, ActivityError> {
        if let Some(secret_name) = name.strip_prefix("secret:") {
            let value = boreal_worker::read_secret(secret_name).map_err(|err| {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, err.to_string())
            })?;
            return Ok(format!("Secret length verified: {}", value.len()));
        }
        Ok(format!("Hello, {name}!"))
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

#[workflow_methods]
impl DemoWorkflow {
    #[init]
    fn new(_ctx: &WorkflowContextView, input: DemoInput) -> Self {
        Self { input }
    }

    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<String> {
        let input = ctx.state(|s| s.input.clone());
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

#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    boreal_worker::run_worker("greet", |queue| {
        Ok(WorkerOptions::new(queue)
            .graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE)
            .register_activities(DemoActivities)
            .register_workflow::<ChildGreetingWorkflow>()?
            .register_workflow::<DemoWorkflow>()?
            .build())
    })
    .await
}
