# Boreal SDK

`boreal-sdk` is the only Boreal crate downstream Rust projects declare. It
handles worker startup and integration with Boreal without changing how you
write Temporal workflows or activities.

- `worker::task_queue(worker)` — prefixes the deployed worker name with the
  configured Temporal namespace. With `TEMPORAL_NAMESPACE=alice`,
  `task_queue("example")?` returns `"alice-example"` without connecting.
- `client::connect()` — connects a Temporal `Client` from standard `TEMPORAL_ADDRESS`,
  `TEMPORAL_NAMESPACE`, and TLS environment settings.
- `client::connect_namespace(namespace)` — same connection settings, bound to
  another user's Temporal namespace. Call it from an activity, then use upstream
  `temporalio_client` `start_workflow`, `get_result`, and `cancel`. The worker
  gRPC gate checks `perms.yaml` using the listener owner, not the namespace
  argument. Do not call it from workflow code.
- `worker::run(worker, build_options)` — derives the queue from the connected
  client's namespace and the deployed worker name. It installs `SIGTERM` and
  `SIGINT` handlers before connecting and continues driving `Worker::run` after
  shutdown so in-flight work can drain.
- `worker::vault_task_queue(namespace)` — returns `<namespace>-vault`. Use
  `ctx.namespace()` and disable eager execution explicitly on Temporal's
  activity options builder.
- `schema` (feature `schema`) — declares and emits a worker's launch catalog.
- `secret::read(name)` / `secret::read_bytes(name)` (feature `read-secrets`) —
  reads user-managed values from a dedicated [vault package](../docs/vault.md).

## Artifacts

Enable a [Nix binary cache](../docs/artifacts.md) for the worker, then use
`artifact::Store::from_env()` in an Activity. On the producing side:

```rust,ignore
let artifact = boreal_sdk::artifact::Store::from_env()?.publish_file(&finished_iso)?;
let result = artifact.as_str().to_owned(); // Return this string through Temporal.
```

On the consuming side, parse the string and hold the returned pin while the
file is in use:

```rust,ignore
let store = boreal_sdk::artifact::Store::from_env()?;
let image = store.fetch(&result.parse()?)?;
provision(image.path())?;
```

These helpers block while Nix runs. Call them on a blocking thread in an async
Activity, never in Workflow code. Only publish non-confidential files.

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

The schedule bin uses `client::connect()` and `worker::task_queue()`, upserts with
`Client::create_schedule` / `ScheduleHandle::update`, and exits. Do not add a
Boreal schedule wrapper. The worker gate allows the stock schedule RPCs, clamps
`catchup_window` to 60 seconds, and rejects a backfill wider than 24 hours.
While an update drain is closed, an unpaused create, unpause, trigger, or
backfill fails and the oneshot retries.

```rust
use boreal_sdk::{client, worker};

let name = std::env::args().nth(1).ok_or("missing worker name")?;
let queue = worker::task_queue(&name)?;
let client = client::connect().await?;
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
`boreal.lib.buildRustWorker` automatically get the pinned SDK linked at
`.boreal/boreal-sdk`. Reference it relative to the relevant `Cargo.toml`, as
in the [starter workspace](../templates/default/temporal/Cargo.toml):

```toml
[dependencies]
boreal-sdk = { path = "../.boreal/boreal-sdk", features = ["schema"] }
```

Keep the usual Temporal and input-type dependencies. In particular, Temporal's
procedural macros refer to upstream crates by name, so they still need direct
Cargo dependencies. With `schema` enabled, declare native workflow
registrations once in `main.rs`:

```rust,ignore
use boreal_sdk::worker;

worker::main! {
    package = "example",
    workflows = {
        export greet: ExampleWorkflow => "Run the starter greeting",
        export CountWorkflow => "Count Unicode scalar values",
        internal ChildGreetingWorkflow,
    },
    configure = |opts| opts.register_activities(ExampleActivities),
}
```

Point both Cargo binaries at that file:

```toml
[[bin]]
name = "example"
path = "main.rs"

[[bin]]
name = "schema"
path = "main.rs"
```

The macro generates `main` and dispatches using Cargo's compile-time binary
name. `example` runs the worker; `schema` prints YAML without connecting to
Temporal. `buildRustWorker` keeps the resulting `schema.yaml`, not the schema
binary. Cargo may warn about the shared source file. In crates with custom
entry points, pass `entrypoint = false,` before `package` to generate only
`worker_options(queue)` and `schema()`.
All three workflows register on the same queue. Only the two `export` entries
are Boreal launch targets, and only their input types need `JsonSchema`.
The `package` key matches the binary output, not the deployed worker name.
`export CountWorkflow => ...` uses the registered Temporal type as the alias;
this example assumes the type is named `count-characters`. Aliases start with
a lowercase ASCII letter and may contain lowercase ASCII letters, digits,
hyphens, and underscores. For other type names, give an explicit alias as in
`export greet: ExampleWorkflow => ...`.

If the `example` package is deployed as `tools`, use
`run tools --workflow count-characters` to open the form (add `--id job-42` to set the
workflow ID explicitly). The `workers` command lists the
available aliases and their arguments. For a worker with one export, omit
`--workflow`. Handwritten single-workflow schemas and `schema::Worker::new`
still work without the macro.

The generated catalog keeps each workflow's full Schemars schema. To skip the
interactive form, pass the input with `--input-json`.
The broker validates against the selected workflow's schema. Keep business
rules and checks not expressed by that schema in the workflow itself.

## Upgrade from `boreal-worker`

Update the pinned Boreal revision and rename the Cargo path dependency to
`boreal-sdk` at `.boreal/boreal-sdk`. Regenerate `Cargo.lock` after entering the
updated dev shell. Replace `declare_worker!` with `worker::main!`, use the
module paths above, and import upstream Temporal crates directly where needed.
For a handwritten schema binary, use `schema::emit`. For vault calls, use
`worker::vault_task_queue` and set `.do_not_eagerly_execute(true)` on the
Temporal builder. The deployed binaries, queues, and schema format do not
change.

## Checks

```sh
cargo fmt --manifest-path sdk/Cargo.toml --all -- --check
cargo test --manifest-path sdk/Cargo.toml --locked
cargo test --manifest-path sdk/Cargo.toml --workspace --all-features --locked
```
