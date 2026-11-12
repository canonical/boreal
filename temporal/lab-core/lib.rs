use std::{env, error::Error, future::Future, time::Duration};

use serde::{Deserialize, Serialize};
use temporalio_client::{
    Client, ClientOptions, Connection, envconfig::LoadClientConfigProfileOptions,
};
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    ActivityExecutionError, ActivityOptions, Runtime, TimerResult, Worker, WorkerOptions,
    WorkflowContext, WorkflowContextView, WorkflowResult, WorkflowTermination,
    activities::{ActivityContext, ActivityError},
};

pub type LabResult<T> = std::result::Result<T, Box<dyn Error>>;
pub const DEFAULT_WAIT_SECONDS: u64 = 20;
pub const MAX_WAIT_SECONDS: u64 = 120 * 60;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

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
    let queue = match env::var("LAB_TASK_QUEUE") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => workload.default_queue().to_owned(),
        Err(err) => return Err(err.into()),
    };
    if queue.trim().is_empty() {
        return Err("LAB_TASK_QUEUE must not be empty".into());
    }
    Ok(queue)
}

/// Uses Temporal's standard TEMPORAL_ADDRESS, TEMPORAL_NAMESPACE and TLS configuration.
pub async fn connect() -> LabResult<Client> {
    let (connection_options, client_options) =
        ClientOptions::load_from_config(LoadClientConfigProfileOptions::default())?;
    let connection = Connection::connect(connection_options).await?;
    Ok(Client::new(connection, client_options)?)
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
        // This is a durable Temporal timer, not a blocking or Tokio sleep.
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

/// Continue polling the worker future after requesting shutdown, so it can drain.
async fn run_until_shutdown<F, S, H, T>(worker: F, signal: S, shutdown: H) -> T
where
    F: Future<Output = T>,
    S: Future<Output = ()>,
    H: FnOnce(),
{
    tokio::pin!(worker);
    tokio::select! {
        result = &mut worker => result,
        () = signal => {
            shutdown();
            worker.await
        }
    }
}

#[cfg(unix)]
pub async fn run_worker(workload: Workload) -> LabResult<()> {
    use tokio::signal::unix::{SignalKind, signal};

    // Install both handlers before connecting; startup can itself take time.
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let stop = async {
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
    };
    tokio::pin!(stop);
    let queue = task_queue(workload)?;
    let runtime = Runtime::from_current_tokio(Default::default())?;
    let client = tokio::select! {
        result = connect() => result?,
        () = &mut stop => return Ok(()),
    };
    let options = match workload {
        Workload::Greet => WorkerOptions::new(queue)
            .graceful_shutdown_period(SHUTDOWN_GRACE)
            .register_activities(DemoActivities)
            .register_workflow::<DemoWorkflow>()?
            .build(),
        Workload::Count => WorkerOptions::new(queue)
            .graceful_shutdown_period(SHUTDOWN_GRACE)
            .register_activities(CountActivities)
            .register_workflow::<CountWorkflow>()?
            .build(),
    };
    let mut worker = Worker::new(&runtime, client, options)?;
    let shutdown = worker.shutdown_handle();
    run_until_shutdown(worker.run(), stop, shutdown).await?;
    Ok(())
}

#[cfg(not(unix))]
pub async fn run_worker(_workload: Workload) -> LabResult<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this worker requires Unix signals",
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, future};

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

    #[tokio::test]
    async fn test_shutdown_requests_drain_and_awaits_worker_completion() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let drained = Cell::new(false);
        let result =
            run_until_shutdown(async { receiver.await.unwrap() }, future::ready(()), || {
                drained.set(true);
                sender.send(42).unwrap();
            })
            .await;
        assert!(drained.get());
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_worker_error_is_not_hidden_by_shutdown() {
        let result = run_until_shutdown(
            future::ready(Err::<(), _>("worker error")),
            future::pending(),
            || panic!("unexpected shutdown"),
        )
        .await;
        assert_eq!(result, Err("worker error"));
    }
}
