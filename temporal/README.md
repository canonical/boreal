# Demo Lab Temporal workflows

Reference downstream Temporal Rust workspace built on `boreal-worker` (linked
at `../.boreal/boreal-worker` by `boreal.lib.mkDevShell` and
`boreal.lib.buildRustWorker`).

| Package | Workload | Registered workflow / activity | Default queue |
| --- | --- | --- | --- |
| `lab-worker` | `greet` | `DemoWorkflow` / `greet` | `lab` |
| `lab-worker-alt` | `count` | `CountWorkflow` / `count_chars` | `lab-count` |

Both workflow and activity names match the original registrations so existing
Temporal histories replay deterministically.

## Checks

Inside `nix develop` (from `downstream/demo-lab`):

```sh
cargo fmt --manifest-path temporal/Cargo.toml --all -- --check
cargo test --manifest-path temporal/Cargo.toml --workspace --locked
```
