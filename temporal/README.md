# Demo Lab Temporal workflows

Reference downstream Temporal Rust workspace built on `boreal-worker` (linked
at `../.boreal/boreal-worker` by `boreal.lib.mkDevShell` and
`boreal.lib.buildRustWorker`).

| Package | Deployed worker | Registered workflow / activity | Alice's queue |
| --- | --- | --- | --- |
| `lab-worker` | `greet` | `DemoWorkflow` / `greet` | `alice-greet` |
| `lab-worker-alt` | `count` | `CountWorkflow` / `count_chars` | `alice-count` |

Each worker's `declare_worker!` invocation defines its registration and schema.
The workers remain separate so `lab.nix` can grant different access to them.
NixOS passes the deployed worker name as the first argument to worker and
schedule binaries. To turn on the optional greeting schedule, set
`boreal.users.alice.workers.greet.schedule.package` to
`demoPackages.lab-worker.scheduler` in `lab.nix` and set `schedule.enable = true`.
It targets `alice-greet`, not the binary name `scheduler`.

## Checks

Inside `nix develop` (from `downstreams/demo-lab`):

```sh
cargo fmt --manifest-path temporal/Cargo.toml --all -- --check
cargo test --manifest-path temporal/Cargo.toml --workspace --locked
```
