# Contributing to Boreal

This guide is for changes to the framework. For a lab or a workflow, start
with the [downstream template guide](templates/default/README.md).

## Develop and test the framework

Use the Nix package manager with flakes enabled on `x86_64-linux` or
`aarch64-linux`. You do not need to run NixOS. From the framework checkout,
enter `nix develop` to get Cargo, Rust, protobuf, and the formatters.

Run the locked Cargo suite for each crate you change. There is no root Cargo
workspace for the framework crates:

```sh
cargo test --manifest-path common/Cargo.toml --locked
cargo test --manifest-path sdk/Cargo.toml --workspace --locked
cargo test --manifest-path broker/Cargo.toml --locked
cargo test --manifest-path shell/Cargo.toml --locked
cargo test --manifest-path admin/Cargo.toml --locked
cargo test --manifest-path updater/Cargo.toml --locked
```

- **SDK features:** See the [SDK checks](sdk/README.md#checks)
  for `schema` and `read-secrets` coverage.
- **Flake and module changes:** Run `nix flake check`. The flake defines
  module, template, and local checks, plus a VM check on `x86_64-linux`.
- **Terminal changes:** Build the shell and run the PTY suite with Python 3.
  On a local terminal outside SSH, `nix run .#boreal-shell` opens the standalone
  mock preview if `/etc/boreal` is absent.

  ```sh
  cargo build --manifest-path shell/Cargo.toml --locked
  python3 tests/shell-pty.py shell/target/debug/boreal-shell
  ```

- **Local runtime changes:** Follow the [rootless Podman smoke test](docs/local.md#smoke-test-from-the-framework-checkout).
- **Docs changes:** Check links and anchors, verify commands against the
  implementation, and run `git diff --check`. Run changed examples when the
  prerequisites are available.

## Work on a downstream from this checkout

A downstream can live in its own repository or on an orphan branch checked
out beneath the gitignored `downstreams/` directory. From the framework root:

```sh
# Create a new downstream worktree from the local starter template:
git worktree add --orphan -b downstreams/my-lab downstreams/my-lab
(cd downstreams/my-lab && nix flake init -t ../.. && git add .)

# Or mount the reference lab, if its branch is available locally:
git worktree add downstreams/demo-lab downstreams/demo-lab
```

To check or launch the reference VM against this framework checkout without
writing the local input override to the lab's lockfile:

```sh
nix flake check ./downstreams/demo-lab --override-input boreal . --no-write-lock-file
nix run ./downstreams/demo-lab#boreal-vm --override-input boreal . --no-write-lock-file
```

See the [VM guide](docs/vm.md) for SSH setup and guest behavior. Keep private
labs in separate private repositories; Git branches on the same remote are
not a read-access boundary.
