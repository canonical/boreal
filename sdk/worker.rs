//! Entrypoints, task queues, and graceful shutdown for Temporal workers.

#[cfg(unix)]
use std::future::Future;
use std::time::Duration;

use temporalio_sdk::WorkerOptions;
#[cfg(unix)]
use temporalio_sdk::{Runtime, Worker};

use crate::client;
#[cfg(feature = "schema")]
use crate::schema;

pub type Result<T> = std::result::Result<T, crate::Error>;

pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

#[cfg(feature = "schema")]
#[doc(hidden)]
#[macro_export]
macro_rules! __worker_main {
    (entrypoint = false, $($declaration:tt)+) => {
        $crate::__worker_main!(@definitions $($declaration)+);
    };
    (package = $package:literal, $($declaration:tt)+) => {
        $crate::__worker_main!(@definitions package = $package, $($declaration)+);

        #[allow(dead_code)] // A declaration can also live in a library module.
        fn main() -> $crate::worker::Result<()> {
            $crate::worker::__run_entrypoint(
                option_env!("CARGO_BIN_NAME"), $package, worker_options, schema,
            )
        }
    };
    (
        @definitions
        package = $package:literal,
        workflows = {
            $( $kind:ident $name:path $( : $workflow:ty => $description:literal )? ),+ $(,)?
        },
        configure = |$builder:ident| $configure:expr $(,)?
    ) => {
        pub fn worker_options(
            queue: ::std::string::String,
        ) -> $crate::worker::Result<$crate::worker::__WorkerOptions> {
            let mut options = {
                let $builder = $crate::worker::__WorkerOptions::new(queue);
                ($configure).build()
            };
            $( $crate::__worker_main!(@register options, $kind $name $(: $workflow => $description)?); )+
            Ok(options)
        }

        pub fn schema() -> $crate::schema::Worker {
            $crate::schema::Worker::from_exports($package, [
                $( $crate::__worker_main!(@export $kind $name $(: $workflow => $description)?), )+
            ].into_iter().flatten().collect())
        }
    };
    (@register $options:ident, export $alias:path : $workflow:ty => $description:literal) => {
        $options.register_workflow::<$workflow>()?
    };
    (@register $options:ident, internal $workflow:path) => {
        $options.register_workflow::<$workflow>()?
    };
    (@export export $alias:path : $workflow:ty => $description:literal) => {
        Some($crate::schema::Workflow::of::<$workflow>(stringify!($alias), $description))
    };
    (@export internal $workflow:path) => { None };
}

/// Defines `worker_options(queue)`, `schema()`, and the Cargo binary entrypoint.
/// Only `export` entries appear in the launch catalog. Use `entrypoint = false`
/// to generate the definitions without `main`.
#[cfg(feature = "schema")]
#[doc(inline)]
pub use crate::__worker_main as main;

#[cfg(feature = "schema")]
#[doc(hidden)]
pub use temporalio_sdk::WorkerOptions as __WorkerOptions;

#[cfg(feature = "schema")]
#[doc(hidden)]
pub fn __run_entrypoint(
    bin: Option<&str>,
    package: &str,
    options: impl FnOnce(String) -> Result<WorkerOptions>,
    schema: impl FnOnce() -> schema::Worker,
) -> Result<()> {
    match bin {
        Some("schema") => Ok(schema::emit(&[schema()])?),
        Some(bin) if bin == package => {
            let worker = std::env::args()
                .nth(1)
                .ok_or_else(|| format!("usage: {package} <worker-name>"))?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(run(&worker, options))
        }
        _ => Err(format!("expected Cargo binary '{package}' or 'schema', got {bin:?}").into()),
    }
}

/// Derives the worker queue from the configured Temporal namespace without connecting.
pub fn task_queue(worker: &str) -> Result<String> {
    let (_, options) = client::load_config()?;
    task_queue_for(&options.namespace, worker)
}

/// Returns the vault task queue for a workflow's namespace. Pass `ctx.namespace()`.
/// Set `do_not_eagerly_execute(true)` on the activity options separately.
pub fn vault_task_queue(namespace: &str) -> String {
    format!("{namespace}-vault")
}

fn task_queue_for(user: &str, worker: &str) -> Result<String> {
    if !valid_name(user) {
        return Err("Boreal user name must match ^[a-z][a-z0-9_-]*$".into());
    }
    if !valid_name(worker) {
        return Err("Boreal worker name must match ^[a-z][a-z0-9_-]*$".into());
    }
    Ok(format!("{user}-{worker}"))
}

fn valid_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// Runs a worker on its deployed task queue and drains in-flight work on shutdown.
#[cfg(unix)]
pub async fn run(
    worker_name: &str,
    build_options: impl FnOnce(String) -> Result<WorkerOptions>,
) -> Result<()> {
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

    let runtime = Runtime::from_current_tokio(Default::default())?;
    let client = tokio::select! {
        result = client::connect() => result?,
        () = &mut stop => return Ok(()),
    };
    let queue = task_queue_for(&client.options().namespace, worker_name)?;
    let options = build_options(queue)?;
    let mut worker = Worker::new(&runtime, client, options)?;
    let shutdown = worker.shutdown_handle();
    run_until_shutdown(worker.run(), stop, shutdown).await?;
    Ok(())
}

#[cfg(not(unix))]
pub async fn run(
    _worker_name: &str,
    _build_options: impl FnOnce(String) -> Result<WorkerOptions>,
) -> Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this worker requires Unix signals",
    )
    .into())
}

// Continue polling after requesting shutdown so in-flight work can drain.
#[cfg(unix)]
async fn run_until_shutdown<T>(
    worker: impl Future<Output = T>,
    signal: impl Future<Output = ()>,
    shutdown: impl FnOnce(),
) -> T {
    tokio::pin!(worker);
    tokio::select! {
        result = &mut worker => result,
        () = signal => {
            shutdown();
            worker.await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_queue_prefixes_deployed_worker_name() {
        assert_eq!(task_queue_for("alice", "example").unwrap(), "alice-example");
        assert_eq!(
            task_queue_for("bob", "nightly_tools").unwrap(),
            "bob-nightly_tools"
        );
        for invalid in ["", " ", "Alice", "1worker", "team/worker"] {
            assert!(task_queue_for(invalid, "tools").is_err());
            assert!(task_queue_for("alice", invalid).is_err());
        }
    }

    #[test]
    fn test_vault_task_queue_uses_workflow_namespace() {
        assert_eq!(vault_task_queue("alice"), "alice-vault");
        assert_eq!(vault_task_queue("local"), "local-vault");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shutdown_requests_drain_and_awaits_worker_completion() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let result = run_until_shutdown(
            async { receiver.await.unwrap() },
            std::future::ready(()),
            || {
                sender.send(42).unwrap();
            },
        )
        .await;
        assert_eq!(result, 42);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_worker_error_is_not_hidden_by_shutdown() {
        let result = run_until_shutdown(
            std::future::ready(Err::<(), _>("worker error")),
            std::future::pending(),
            || panic!("unexpected shutdown"),
        )
        .await;
        assert_eq!(result, Err("worker error"));
    }
}
