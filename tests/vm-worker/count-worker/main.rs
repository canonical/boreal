use std::time::Duration;

use serde::{Deserialize, Serialize};
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    ActivityExecutionError, ActivityOptions, TimerResult, WorkerOptions, WorkflowContext,
    WorkflowContextView, WorkflowResult, WorkflowTermination,
    activities::{ActivityContext, ActivityError},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DemoInput {
    pub name: String,
    pub wait_seconds: u64,
}

pub struct CountActivities;

#[activities]
impl CountActivities {
    #[activity]
    pub async fn count_chars(_ctx: ActivityContext, name: String) -> Result<String, ActivityError> {
        Ok(format!("Character count: {}", name.chars().count()))
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

#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    boreal_worker::run_worker("count", |queue| {
        Ok(WorkerOptions::new(queue)
            .graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE)
            .register_activities(CountActivities)
            .register_workflow::<CountWorkflow>()?
            .build())
    })
    .await
}
