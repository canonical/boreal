use std::time::Duration;

use serde::{Deserialize, Serialize};
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    ActivityExecutionError, ActivityOptions, WorkerOptions, WorkflowContext, WorkflowContextView,
    WorkflowResult, WorkflowTermination,
    activities::{ActivityContext, ActivityError},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExampleInput {
    pub message: String,
}

fn format_reply(message: &str) -> Result<String, &'static str> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return Err("message must not be empty");
    }
    Ok(format!("Processed: {trimmed}"))
}

pub struct ExampleActivities;

#[activities]
impl ExampleActivities {
    #[activity]
    pub async fn echo(_ctx: ActivityContext, message: String) -> Result<String, ActivityError> {
        format_reply(&message)
            .map_err(|msg| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg).into())
    }
}

#[workflow]
pub struct ExampleWorkflow {
    input: ExampleInput,
}

#[workflow_methods]
impl ExampleWorkflow {
    #[init]
    fn new(_ctx: &WorkflowContextView, input: ExampleInput) -> Self {
        Self { input }
    }

    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<String> {
        let input = ctx.state(|state| state.input.clone());
        ctx.execute_activity(
            ExampleActivities::echo,
            input.message,
            ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
        )
        .await
        .map_err(|error| match error {
            ActivityExecutionError::Cancelled(_) => WorkflowTermination::cancelled(),
            other => other.into(),
        })
    }
}

#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    boreal_worker::run_worker("example", |queue| {
        Ok(WorkerOptions::new(queue)
            .graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE)
            .register_activities(ExampleActivities)
            .register_workflow::<ExampleWorkflow>()?
            .build())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_formats_message_and_rejects_blank_input() {
        assert_eq!(format_reply("hello").unwrap(), "Processed: hello");
        assert!(format_reply("   ").is_err());
    }
}
