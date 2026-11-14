# Boreal framework

A NixOS and Temporal Rust framework for SSH-managed multi-user labs. `main`
provides the reusable NixOS modules, Nix builder library (`boreal.lib`),
`boreal-worker` Rust runtime crate, interactive `boreal-shell` prototype, and
fail-closed `boreal-updater` service. Downstream lab deployments live on orphan
branches checked out as worktrees under gitignored `downstream/` (for example,
`downstream/demo-lab` on orphan branch `demo-lab`).

The root flake tracks `nixos-26.05`; `flake.lock` pins the selected revision.
Update lock files deliberately rather than resolving moving branches during
deployment.

**This is not yet a secure shared-lab deployment.** Although Boreal generates
authoritative `/etc/boreal/workers.yaml` and `/etc/boreal/users/<user>/perms.yaml`
catalogs, runtime permission enforcement and Temporal server/host isolation are
not wired up yet. A development server on localhost is reachable by local users,
and namespaces, Unix accounts, and agent skills do not by themselves enforce
Temporal isolation.

The update service can schedule and prebuild a pinned revision, but it **never
switches the system**: it fails closed until an independently enforced
admission-and-drain protocol exists. It does not run garbage collection. See
[architecture](docs/architecture.md) and [updater](updater/README.md).

## Framework development

```sh
nix develop
cargo test --manifest-path worker/Cargo.toml --locked
cargo test --manifest-path updater/Cargo.toml --locked
cargo test --manifest-path shell/Cargo.toml --locked
nix flake check
```

For the shell's terminal interaction checks:

```sh
cargo build --manifest-path shell/Cargo.toml --locked
python3 tests/shell-pty.py shell/target/debug/boreal-shell
```

Run the mock interactive shell locally without a VM or Temporal server:

```sh
nix run .#boreal-shell
```

Commands in `boreal-shell` currently display parsed mock arguments only; they
never connect to Temporal, run programs, or enforce security boundaries.

## Downstream lab worktrees

Downstream projects consume Boreal as a flake input and live on independent
orphan branches mounted inside gitignored `downstream/<name>/`:

```sh
# Mount the reference demo-lab orphan branch:
git worktree add downstream/demo-lab demo-lab

# Check or run the downstream VM against local framework commits on main:
nix flake check ./downstream/demo-lab --override-input boreal .
nix run ./downstream/demo-lab#boreal-vm --override-input boreal .
```

Note that Git remotes do not enforce per-branch read permissions: keep public
examples (like `demo-lab`) as orphan branches on `origin`, and push private labs
to separate private Git remotes.

## Flake outputs

- `lib`:
  - `mkHost { system, modules, specialArgs ? {} }` — NixOS system preloaded with
    `nixosModules.default`.
  - `mkVm { system ? "x86_64-linux", modules, specialArgs ? {} }` — disposable
    QEMU guest preloaded with `nixosModules.default` and `nixosModules.vm`.
  - `mkLabCheck { pkgs, modules, specialArgs ? {} }` — `nix flake check`
    derivation verifying module assertions and secret-free `/etc/boreal/` YAML
    generation.
  - `mkDevShell pkgs { ... }` and `buildRustWorker pkgs { ... }` — Rust
    development shell and Temporal worker package builder linking
    `${boreal}/worker` at `.boreal/boreal-worker` without manual `outputHashes`.
- `nixosModules`:
  - `default` / `boreal` — `boreal.enable`, `boreal.workerPackages`,
    `boreal.users`, and `boreal.updater`.
  - `vm` — disposable QEMU guest configuration (`boreal.vm.*`) with dynamic
    `temporal-dev` namespaces derived from `boreal.users`.
  - `chinaMirrors` — opt-in BFSU and SJTU Nix substituters.
- `packages.<system>`: `boreal-shell` and `boreal-updater`.

See [host configuration](docs/host.md), [VM guide](docs/vm.md), and
[worker runtime](worker/README.md) for downstream usage details.

Users should provision their own secrets outside Git and the Nix store under
`~/.config/boreal/secrets` (`0700` directory, `0600` files). Never place secret
contents into Temporal inputs, results, errors, or logs. Agent instructions are
in `AGENTS.md` and `.agents/skills/`.
