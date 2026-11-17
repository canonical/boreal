# `boreal-worker` runtime crate

`boreal-worker` provides the shared Unix Temporal worker lifecycle used by
downstream Boreal Rust workers:

- `task_queue(default_queue)` — resolves `LAB_TASK_QUEUE` when set, falling back
  to `default_queue` and rejecting blank names.
- `connect()` — connects a Temporal `Client` from standard `TEMPORAL_ADDRESS`,
  `TEMPORAL_NAMESPACE`, and TLS environment settings.
- `connect_namespace(namespace)` — same connection settings, bound to another
  user's Temporal namespace. Call it from an activity, then use upstream
  `temporalio_client` `start_workflow`, `get_result`, and `cancel`. The worker
  gRPC gate checks `perms.yaml` using the listener owner, not the namespace
  argument. Do not call it from workflow code.
- `run_worker(default_queue, build_options)` — installs `SIGTERM` and `SIGINT`
  handlers before connecting, builds a `Worker` on the current Tokio runtime,
  and continues driving `Worker::run` after shutdown so in-flight work can drain
  within `DEFAULT_SHUTDOWN_GRACE` (30 seconds).
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
`boreal-schedule-<user>-<worker>.service` when `schedule.enable` is true.

The schedule bin uses `connect()` and `task_queue()`, upserts with
`Client::create_schedule` / `ScheduleHandle::update`, and exits. Do not add a
Boreal schedule wrapper. The worker gate allows the stock schedule RPCs, clamps
`catchup_window` to 60 seconds, and rejects a backfill wider than 24 hours.
While an update drain is closed, an unpaused create, unpause, trigger, or
backfill fails and the oneshot retries.

```rust
let client = boreal_worker::connect().await?;
let queue = boreal_worker::task_queue("hello")?;
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

Enable the `schema` feature and add a `schema` bin that calls
`boreal_worker::emit_schema`. `buildRustWorker` runs that bin and keeps
`schema.yaml`, not the binary. Input structs derive `JsonSchema` and `Validate`.
Call `input.validate()?` at the start of the workflow run. Range, length, and
pattern checks live there; the shell only coerces positional tokens to JSON
types. The YAML key is `workflow`, not `workflowType`.

## Checks

```sh
cargo fmt --manifest-path worker/Cargo.toml -- --check
cargo test --manifest-path worker/Cargo.toml --locked
```
