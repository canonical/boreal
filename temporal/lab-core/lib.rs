use std::time::Duration;

pub use boreal_worker::{Result as LabResult, connect};
use serde::{Deserialize, Serialize};
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    ActivityExecutionError, ActivityOptions, TimerResult, WorkerOptions, WorkflowContext,
    WorkflowContextView, WorkflowResult, WorkflowTermination,
    activities::{ActivityContext, ActivityError},
};

pub const DEFAULT_WAIT_SECONDS: u64 = 20;
pub const MAX_WAIT_SECONDS: u64 = 120 * 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DemoInput {
    pub name: String,
    pub wait_seconds: u64,
}

pub fn validate_input(name: String, wait_seconds: u64) -> LabResult<DemoInput> {
    if name.trim().is_empty() {
        return Err("name must not be empty".into());
    }
    if !(1..=MAX_WAIT_SECONDS).contains(&wait_seconds) {
        return Err("wait seconds must be between 1 and 7200".into());
    }
    Ok(DemoInput { name, wait_seconds })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Workload {
    Greet,
    Count,
}

impl Workload {
    pub fn default_queue(self) -> &'static str {
        match self {
            Self::Greet => "lab",
            Self::Count => "lab-count",
        }
    }
}

pub fn task_queue(workload: Workload) -> LabResult<String> {
    boreal_worker::task_queue(workload.default_queue())
}

pub struct DemoActivities;

#[activities]
impl DemoActivities {
    #[activity]
    pub async fn greet(
        _ctx: ActivityContext,
        name: String,
    ) -> std::result::Result<String, ActivityError> {
        Ok(format!("Hello, {name}!"))
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
        let input = ctx.state(|state| state.input.clone());
        // Durable Temporal timer persisted in event history.
        if ctx.timer(Duration::from_secs(input.wait_seconds)).await == TimerResult::Cancelled {
            return Err(WorkflowTermination::cancelled());
        }
        let greeting = ctx
            .execute_activity(
                DemoActivities::greet,
                input.name,
                ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
            )
            .await
            .map_err(|error| match error {
                ActivityExecutionError::Cancelled(_) => WorkflowTermination::cancelled(),
                other => other.into(),
            })?;
        Ok(greeting)
    }
}

pub struct CountActivities;

fn character_count(name: &str) -> String {
    format!("Character count: {}", name.chars().count())
}

#[activities]
impl CountActivities {
    #[activity]
    pub async fn count_chars(
        _ctx: ActivityContext,
        name: String,
    ) -> std::result::Result<String, ActivityError> {
        Ok(character_count(&name))
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
        let input = ctx.state(|state| state.input.clone());
        if ctx.timer(Duration::from_secs(input.wait_seconds)).await == TimerResult::Cancelled {
            return Err(WorkflowTermination::cancelled());
        }
        let count = ctx
            .execute_activity(
                CountActivities::count_chars,
                input.name,
                ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
            )
            .await
            .map_err(|error| match error {
                ActivityExecutionError::Cancelled(_) => WorkflowTermination::cancelled(),
                other => other.into(),
            })?;
        Ok(count)
    }
}

pub async fn run_worker(workload: Workload) -> LabResult<()> {
    boreal_worker::run_worker(workload.default_queue(), |queue| {
        let options = match workload {
            Workload::Greet => WorkerOptions::new(queue)
                .graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE)
                .register_activities(DemoActivities)
                .register_workflow::<DemoWorkflow>()?
                .build(),
            Workload::Count => WorkerOptions::new(queue)
                .graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE)
                .register_activities(CountActivities)
                .register_workflow::<CountWorkflow>()?
                .build(),
        };
        Ok(options)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_limits_are_checked() {
        assert!(validate_input(" ".into(), 1).is_err());
        assert!(validate_input("A".into(), 0).is_err());
        assert!(validate_input("A".into(), MAX_WAIT_SECONDS + 1).is_err());
        assert_eq!(
            validate_input("A".into(), MAX_WAIT_SECONDS)
                .unwrap()
                .wait_seconds,
            7200
        );
    }

    #[test]
    fn test_workloads_use_distinct_default_queues() {
        assert_eq!(Workload::Greet.default_queue(), "lab");
        assert_eq!(Workload::Count.default_queue(), "lab-count");
        assert_ne!(
            Workload::Greet.default_queue(),
            Workload::Count.default_queue()
        );
    }

    #[test]
    fn test_count_activity_counts_unicode_scalars_not_bytes() {
        assert_eq!(character_count("Élan"), "Character count: 4");
        assert_eq!(character_count(""), "Character count: 0");
    }
}
