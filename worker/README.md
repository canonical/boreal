# `boreal-worker` runtime crate

`boreal-worker` provides the shared Unix Temporal worker lifecycle used by
downstream Boreal Rust workers:

- `task_queue(worker)` — prefixes the deployed worker name with the configured
  Temporal namespace. With `TEMPORAL_NAMESPACE=alice`, `task_queue("example")?`
  returns `"alice-example"`. No Temporal connection, macro, or runtime wrapper
  is required.
- `connect()` — connects a Temporal `Client` from standard `TEMPORAL_ADDRESS`,
  `TEMPORAL_NAMESPACE`, and TLS environment settings.
- `connect_namespace(namespace)` — same connection settings, bound to another
  user's Temporal namespace. Call it from an activity, then use upstream
  `temporalio_client` `start_workflow`, `get_result`, and `cancel`. The worker
  gRPC gate checks `perms.yaml` using the listener owner, not the namespace
  argument. Do not call it from workflow code.
- `run_worker(worker, build_options)` — derives the queue from the connected
  client's namespace and the deployed worker name. It installs `SIGTERM` and
  `SIGINT` handlers before connecting and continues driving `Worker::run` after
  shutdown so in-flight work can drain.
- `read_secret(name)` / `read_secret_bytes(name)` — loads a user-managed secret
  from `~/.config/boreal/secrets/<name>` after verifying `0700` directory and
  `0600` regular-file ownership. Never place returned secret values in Temporal
  inputs, results, errors, heartbeats, or logs.
- Re-exports `temporalio_client`, `temporalio_common`, `temporalio_macros`,
  `temporalio_sdk`, and `temporalio_workflow`.

## Schedules

A schedule is a Temporal server record, not a long-running process. Downstream
packages that want one compile a bin such as `greet-schedule` and pass
`outputs = [ "out" "greet" "greet-schedule" "schema" ]` to
`boreal.lib.buildRustWorker`. NixOS starts that binary once per user-worker as
`boreal-schedule-<user>-<worker>.service` when `schedule.enable` is true. Both
the worker and schedule binaries receive the deployed worker name as their
first argument. This is the key in `boreal.users.<user>.workers`, not the binary
name or the full queue. Migrate both entrypoints together when upgrading from
`LAB_TASK_QUEUE`.

The schedule bin uses `connect()` and `task_queue()`, upserts with
`Client::create_schedule` / `ScheduleHandle::update`, and exits. Do not add a
Boreal schedule wrapper. The worker gate allows the stock schedule RPCs, clamps
`catchup_window` to 60 seconds, and rejects a backfill wider than 24 hours.
While an update drain is closed, an unpaused create, unpause, trigger, or
backfill fails and the oneshot retries.

```rust
let worker = std::env::args().nth(1).ok_or("missing worker name")?;
let queue = boreal_worker::task_queue(&worker)?;
let client = boreal_worker::connect().await?;
client
    .create_schedule(
        "hourly-hello",
        CreateScheduleOptions::builder()
            .action(ScheduleAction::start_workflow::<HelloWorkflow>(
                input,
                queue,
                "hourly-hello",
            ))
            .spec(ScheduleSpec::from_interval(Duration::from_hours(1)))
            .build(),
    )
    .await?;
```

`AlreadyExists` does not apply a changed spec. Describe and update in place.

## Downstream usage

Downstream flakes using `boreal.lib.mkDevShell` and
`boreal.lib.buildRustWorker` automatically receive `${boreal}/worker` linked at
`.boreal/boreal-worker`, allowing `Cargo.toml` to reference it as a local path
dependency without maintaining Git `outputHashes` in `flake.nix`:

```toml
[dependencies]
boreal-worker = { path = "../.boreal/boreal-worker", features = ["schema"] }
schemars = { version = "1", features = ["preserve_order"] }
serde = { version = "1", features = ["derive"] }
validator = { version = "0.20", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

With the `schema` feature, declare ordinary native workflow registrations once
in the worker library:

```rust,ignore
boreal_worker::declare_worker! {
    package = "example",
    workflows = {
        export greet: ExampleWorkflow => "Run the starter greeting",
        export count: CountWorkflow => "Count Unicode scalar values",
        internal ChildGreetingWorkflow,
    },
    configure = |opts| opts.register_activities(ExampleActivities),
}
```

The worker binary uses the generated `worker_options(queue)`. The schema binary
calls `boreal_worker::emit_schema(&[example_worker::schema()])`;
`buildRustWorker` keeps the resulting `schema.yaml`, not the binary.
All three workflows register on the same queue. Only the two `export` entries
are Boreal launch targets, and only their input types need `JsonSchema`.
The `package` key matches the binary output, not the deployed worker name.

If the `example` package is deployed as `tools`, use
`run tools --workflow count` to open the form (add `--id job-42` to set the
workflow ID explicitly). The `workers` command lists the
available aliases and their arguments. For a worker with one export, omit
`--workflow`. Handwritten single-workflow schemas and `WorkerSchema::new`
still work without the macro.

The generated catalog keeps each workflow's full Schemars schema. To skip the
interactive form, pass the input with `--input-json`.
The broker validates against the selected workflow's schema. Keep business
rules and checks not expressed by that schema in the workflow itself.

## Checks

```sh
cargo fmt --manifest-path worker/Cargo.toml -- --check
cargo test --manifest-path worker/Cargo.toml --locked
```
