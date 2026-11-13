# Boreal Temporal workflows

This workspace uses the official Temporal Rust SDK 1.2. `lab-core` contains the
shared worker setup and the two Boreal workflows and activities (`greet` and
`count`). Their registered Temporal workflow and activity type names are
preserved for deterministic replay compatibility with existing histories, and
each separately deployable worker registers only its own workflow and activity:

| Package | Client workload | Workflow / activity | Default queue |
| --- | --- | --- | --- |
| `lab-worker` | `greet` | `DemoWorkflow` / `greet` | `lab` |
| `lab-worker-alt` | `count` | `CountWorkflow` / `count_chars` | `lab-count` |

The count activity counts Unicode scalar values, not bytes or grapheme clusters.
`lab-client` starts and describes either workflow. These are separate processes,
not isolation or authorization boundaries. Run workers under the intended user
accounts and configure and enforce namespace access on the Temporal server and host.

Each demo schedules a durable Temporal timer (20 seconds by default; the demo
client accepts 1 to 7,200 seconds), then runs its short activity. A 30–120 minute
demonstration can use 1,800–7,200 seconds without tying up a worker thread.
The timer is persisted in Temporal history. Workflow replay is fundamental to
Temporal and is **not optional or disabled** here; workflow code must remain
deterministic and changes must be compatible with existing histories. Production
upgrades need replay testing/versioning, not just this example. Activity work
may be retried, so real side effects must be idempotent.

## Run locally

Requirements: Rust 1.92 or newer, `cargo`, `protoc`, and a Temporal server.
The `temporal` CLI is optional but can start a local development server. From
this directory, build with `cargo build --workspace --locked`. For example:

```sh
temporal server start-dev --headless
```

In separate terminals, with the same namespace (and no `LAB_TASK_QUEUE` override):

```sh
export TEMPORAL_ADDRESS=localhost:7233
export TEMPORAL_NAMESPACE=default
cargo run --locked -p lab-worker
```

```sh
export TEMPORAL_ADDRESS=localhost:7233
export TEMPORAL_NAMESPACE=default
cargo run --locked -p lab-worker-alt
```

```sh
export TEMPORAL_ADDRESS=localhost:7233
export TEMPORAL_NAMESPACE=default
cargo run --locked -p lab-client -- start greet demo-1 Example 20
cargo run --locked -p lab-client -- start count demo-2 Example 20
cargo run --locked -p lab-client -- status demo-1
cargo run --locked -p lab-client -- status demo-2
```

Use a unique workflow ID. `status` describes the latest run for that ID, not
necessarily a particular prior run. The client prints the run ID on start.
The Temporal UI or CLI can inspect results and history. Workflow input, activity
input/result, and IDs are persisted in Temporal; use only non-sensitive example
values. Never supply secrets in arguments or workflow data. The SDK reads its
standard `TEMPORAL_ADDRESS`, `TEMPORAL_NAMESPACE`, and TLS settings from the
environment or profile; see the
[SDK client configuration](https://docs.temporal.io/develop/rust/client/temporal-client).
Each client start uses the selected workload's default queue. `LAB_TASK_QUEUE`
overrides that queue for the current process; set the same value on the client
and the matching worker when using a custom queue. **Never configure `lab-worker`
and `lab-worker-alt` to poll the same queue:** Temporal does not route tasks by
registered type, so differently registered workers on one queue can receive and
fail each other's tasks. The defaults are distinct, but overrides are not
cross-process validated. Only workers with compatible registrations should poll
the same queue.

For a manual recovery demonstration, start a workflow with a 60-second timer,
stop its matching worker with SIGTERM or Ctrl-C, restart that same worker, and
inspect its status after the timer expires. The other worker cannot process it.
The service must persist history; the development server's
default in-memory database does not survive a server restart. Do not interrupt
other users' work when performing this test.

## Shutdown and updates

Both worker binaries install SIGTERM and SIGINT handlers before connecting.
On a signal they initiate the SDK shutdown and continue driving `Worker::run`
until it returns. The SDK stops polling and allows in-flight work to drain; the
configured 30-second grace period triggers cancellation of outstanding
activities after it expires. It is not a hard deadline: Core can wait for an
activity that ignores cancellation. The demo activities are short, but real
activities must handle cancellation, retries, and idempotence. For systemd,
run one foreground process per user, send SIGTERM and choose a
`TimeoutStopSec` longer than the grace period plus a suitable drain margin;
forced termination may interrupt work. Workflow executions and timers can
continue on a compatible worker later; shutdown does not complete or cancel
all workflows.

**This client does not enforce maintenance.** Stopping workers or pausing
polling does not prevent new workflow starts: the server can accept and queue
them. A host-side updater must fail closed until an independently enforced start
gate is in place, including every start path (other clients, schedules,
signal-with-start and child workflows as applicable). It must close that gate
atomically with respect to in-flight starts and account for already accepted
starts before checking/draining work or replacing binaries. Reopen only after
the update and compatible workers are ready. Neither this client nor queue names
provide that gate, and an unavailable or uncertain gate is not permission to
proceed. Coordinate the actual maintenance protocol with the host/server design.

## Checks

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
```

The unit tests cover argument validation and shutdown orchestration without a
server. They do not prove live SDK replay, signal handling, systemd behavior,
or maintenance safety; verify those against a disposable Temporal instance.
