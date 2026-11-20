use std::time::Duration;

pub use boreal_worker::{Result as LabResult, connect, task_queue};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    ActivityExecutionError, ActivityOptions, ApplicationFailure, TimerResult, WorkflowContext,
    WorkflowContextView, WorkflowResult, WorkflowTermination,
    activities::{ActivityContext, ActivityError},
};

pub mod vault;

pub const DEFAULT_WAIT_SECONDS: u64 = 20;
pub const MAX_WAIT_SECONDS: u64 = 120 * 60;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DemoInput {
    /// Recipient name or input text to count
    pub name: String,
    /// Durable timer duration in seconds
    #[serde(default = "default_wait_seconds")]
    pub wait_seconds: u64,
    /// Ask the central vault to verify the demo token (alice/greet only).
    #[serde(default)]
    pub verify_secret: bool,
}

fn default_wait_seconds() -> u64 {
    DEFAULT_WAIT_SECONDS
}

pub fn validate_input(name: String, wait_seconds: u64) -> LabResult<DemoInput> {
    let input = DemoInput {
        name,
        wait_seconds,
        verify_secret: false,
    };
    input.validate()?;
    Ok(input)
}

impl DemoInput {
    fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty() {
            return Err("name must not be empty");
        }
        if !(1..=MAX_WAIT_SECONDS).contains(&self.wait_seconds) {
            return Err("wait seconds must be between 1 and 7200");
        }
        Ok(())
    }
}

fn validate_workflow_input(input: &DemoInput) -> Result<(), WorkflowTermination> {
    input.validate().map_err(|message| {
        WorkflowTermination::failed_application(ApplicationFailure::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message,
        )))
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Workload {
    Greet,
    Count,
}

impl Workload {
    pub fn worker_name(self) -> &'static str {
        match self {
            Self::Greet => "greet",
            Self::Count => "count",
        }
    }
}

pub struct DemoActivities;

fn greeting(name: &str, hostname: &str) -> String {
    format!("Hello {name} from {}.", hostname.trim_end())
}

#[activities]
impl DemoActivities {
    #[activity]
    pub async fn greet(
        _ctx: ActivityContext,
        name: String,
    ) -> std::result::Result<String, ActivityError> {
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")?;
        Ok(greeting(&name, &hostname))
    }
}

#[workflow]
pub struct DemoWorkflow {
    input: DemoInput,
}

impl DemoWorkflow {
    /// Marker for `ScheduleAction::start_workflow`. The schedule payload is passed separately.
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
        let input = ctx.state(|state| state.input.clone());
        validate_workflow_input(&input)?;
        // Durable Temporal timer persisted in event history.
        if ctx.timer(Duration::from_secs(input.wait_seconds)).await == TimerResult::Cancelled {
            return Err(WorkflowTermination::cancelled());
        }
        if input.verify_secret {
            let options = boreal_worker::vault_activity_options(
                ctx.namespace(),
                ActivityOptions::schedule_to_close_timeout(Duration::from_secs(10)),
            );
            ctx.execute_activity(vault::Vault::verify_demo_token, (), options)
                .await
                .map_err(|error| match error {
                    ActivityExecutionError::Cancelled(_) => WorkflowTermination::cancelled(),
                    other => other.into(),
                })?;
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
        validate_workflow_input(&input)?;
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

pub async fn register_greet_schedule(worker_name: &str) -> LabResult<()> {
    use boreal_worker::temporalio_client::schedules::{
        CreateScheduleOptions, ScheduleAction, ScheduleError, ScheduleOverlapPolicy, ScheduleSpec,
    };

    const SCHEDULE_ID: &str = "greet-30s";

    fn already_exists(err: &ScheduleError) -> bool {
        match err {
            ScheduleError::Rpc(status) => status.code() as i32 == 6,
            _ => false,
        }
    }

    let queue = task_queue(worker_name)?;
    let client = connect().await?;
    let spec = ScheduleSpec::from_interval(Duration::from_secs(30));
    let input = DemoInput {
        name: "Scheduler".into(),
        wait_seconds: 1,
        verify_secret: false,
    };
    let action = ScheduleAction::start_workflow(
        DemoWorkflow::scheduled(input.clone()),
        input,
        queue,
        SCHEDULE_ID,
    );
    let opts = CreateScheduleOptions::builder()
        .action(action.clone())
        .spec(spec.clone())
        .overlap_policy(ScheduleOverlapPolicy::Skip)
        .build();
    match client.create_schedule(SCHEDULE_ID, opts).await {
        Ok(_) => Ok(()),
        Err(err) if already_exists(&err) => client
            .get_schedule_handle(SCHEDULE_ID)
            .update(
                move |update| {
                    update
                        .set_spec(spec)
                        .set_action(action)
                        .set_paused(false)
                        .set_overlap_policy(ScheduleOverlapPolicy::Skip)
                        .set_catchup_window(Duration::from_secs(60));
                },
                Default::default(),
            )
            .await
            .map_err(Into::into),
        Err(err) => Err(err.into()),
    }
}

pub mod greet {
    use super::{DemoActivities, DemoWorkflow};

    boreal_worker::declare_worker! {
        package = "lab-worker",
        workflows = {
            export greet: DemoWorkflow => "Schedule a durable timer and greet the named recipient",
        },
        configure = |opts| opts
            .graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE)
            .register_activities(DemoActivities),
    }
}

pub mod count {
    use super::{CountActivities, CountWorkflow};

    boreal_worker::declare_worker! {
        package = "lab-worker-alt",
        workflows = {
            export count: CountWorkflow => "Schedule a durable timer and count Unicode scalar values",
        },
        configure = |opts| opts
            .graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE)
            .register_activities(CountActivities),
    }
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
    fn test_greeting_includes_activity_hostname() {
        assert_eq!(
            greeting("Replica", "boreal-vm\n"),
            "Hello Replica from boreal-vm."
        );
        assert_eq!(
            greeting("Replica", "compute-01\n"),
            "Hello Replica from compute-01."
        );
    }

    #[test]
    fn test_count_activity_counts_unicode_scalars_not_bytes() {
        assert_eq!(character_count("Élan"), "Character count: 4");
        assert_eq!(character_count(""), "Character count: 0");
    }
}
