use std::{env, error::Error, future::Future, time::Duration};

pub use temporalio_client;
pub use temporalio_common;
pub use temporalio_macros;
pub use temporalio_sdk;
pub use temporalio_workflow;

use temporalio_client::{
    Client, ClientOptions, Connection, envconfig::LoadClientConfigProfileOptions,
};
use temporalio_sdk::{Runtime, Worker, WorkerOptions};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

fn resolve_queue(override_queue: Option<String>, default_queue: &str) -> Result<String> {
    let queue = override_queue.unwrap_or_else(|| default_queue.to_owned());
    if queue.trim().is_empty() {
        return Err("task queue must not be empty".into());
    }
    Ok(queue)
}

pub fn task_queue(default_queue: &str) -> Result<String> {
    let override_queue = match env::var("LAB_TASK_QUEUE") {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(err) => return Err(err.into()),
    };
    resolve_queue(override_queue, default_queue)
}

/// Connects using Temporal's standard TEMPORAL_ADDRESS, TEMPORAL_NAMESPACE, and TLS settings.
pub async fn connect() -> Result<Client> {
    let (connection_options, client_options) =
        ClientOptions::load_from_config(LoadClientConfigProfileOptions::default())?;
    let connection = Connection::connect(connection_options).await?;
    Ok(Client::new(connection, client_options)?)
}

/// Continues polling the worker future after requesting shutdown so in-flight work can drain.
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
pub async fn run_worker<F>(default_queue: &str, build_options: F) -> Result<()>
where
    F: FnOnce(String) -> Result<WorkerOptions>,
{
    use tokio::signal::unix::{SignalKind, signal};

    // Install signal handlers before connecting; connection setup can itself block.
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let stop = async {
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
    };
    tokio::pin!(stop);

    let queue = task_queue(default_queue)?;
    let options = build_options(queue)?;
    let runtime = Runtime::from_current_tokio(Default::default())?;
    let client = tokio::select! {
        result = connect() => result?,
        () = &mut stop => return Ok(()),
    };
    let mut worker = Worker::new(&runtime, client, options)?;
    let shutdown = worker.shutdown_handle();
    run_until_shutdown(worker.run(), stop, shutdown).await?;
    Ok(())
}

#[cfg(not(unix))]
pub async fn run_worker<F>(_default_queue: &str, _build_options: F) -> Result<()>
where
    F: FnOnce(String) -> Result<WorkerOptions>,
{
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
    fn test_task_queue_uses_default_or_validates_override() {
        assert_eq!(resolve_queue(None, "lab").unwrap(), "lab");
        assert_eq!(
            resolve_queue(Some("alice-greet".into()), "lab").unwrap(),
            "alice-greet"
        );
        assert!(resolve_queue(Some("  ".into()), "lab").is_err());
        assert!(resolve_queue(None, " ").is_err());
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
