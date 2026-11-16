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

## Downstream usage

Downstream flakes using `boreal.lib.mkDevShell` and
`boreal.lib.buildRustWorker` automatically receive `${boreal}/worker` linked at
`.boreal/boreal-worker`, allowing `Cargo.toml` to reference it as a local path
dependency without maintaining Git `outputHashes` in `flake.nix`:

```toml
[dependencies]
boreal-worker = { path = "../.boreal/boreal-worker" }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Checks

```sh
cargo fmt --manifest-path worker/Cargo.toml -- --check
cargo test --manifest-path worker/Cargo.toml --locked
```
