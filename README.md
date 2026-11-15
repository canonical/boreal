# Boreal framework

A NixOS and Temporal Rust framework for SSH-managed multi-user labs. `main`
provides the reusable NixOS modules, Nix builder library (`boreal.lib`),
starter flake template (`templates.default`), `boreal-common` policy/IPC
library, `boreal-worker` Rust runtime crate, `boreal-broker` policy engine and
worker gRPC gate, interactive `boreal-shell`, restricted `boreal-admin`
interface, and fail-closed `boreal-updater` service. Concrete lab deployments
consume Boreal as a flake input—either in a standalone repository or on
`downstreams/<name>` orphan branches checked out as worktrees under gitignored
`downstreams/` (for example, `downstreams/demo-lab`).

The root flake tracks `nixos-26.05`; `flake.lock` pins the selected revision.
Update lock files deliberately rather than resolving moving branches during
deployment.

## Security and isolation summary

- **Host-isolated Temporal & PostgreSQL**: `services.temporal` (`temporal-server`)
  and PostgreSQL run inside an isolated network namespace
  (`/run/netns/boreal-internal`) with a `0770 postgres:boreal-pg` Unix socket.
  User SSH sessions have no network or Unix-socket route to Temporal or
  PostgreSQL.
- **Per-worker network namespace & gRPC gate**: Each
  `boreal-worker-<user>-<worker>.service` runs under `<user>` inside
  `/run/netns/boreal-worker-<user>-<worker>` with `ProtectProc = "invisible"`
  and `kernel.yama.ptrace_scope = 2`. Workers reach Temporal solely through a
  per-worker HTTP/2 gRPC gate in `boreal-broker` that restricts RPC methods,
  `namespace` (`<user>`), and `task_queue` (`<user>-<worker>`).
- **Kernel-authenticated runtime policy**: `boreal-broker` authenticates callers
  on `/run/boreal/broker.sock` (`0660 boreal-broker:boreal`) via `SO_PEERCRED`,
  validates typed argument schemas from `/etc/boreal/workers.yaml`, enforces
  `/etc/boreal/users/<user>/perms.yaml` across `start`, `cancel`, `status`,
  `result`, `list`, and `permissions`, and verifies registered `workflowType`
  and pinned `run_id` on existing executions.
- **Restricted SSH shell & user secrets**: OpenSSH disables SFTP, TCP/Unix/agent
  forwarding, tunnels, and `~/.ssh/rc`. `boreal-shell` drives commands and
  permission-filtered completions from `/etc/boreal/`, requires `[y/N]`
  confirmation before `cancel`, rejects unsupported non-interactive SSH
  commands, and stores secrets under `~/.config/boreal/secrets` (`0700`/`0600`)
  via hidden TTY prompt or `ssh <user>@<host> "secrets set <name>" < file`.
- **Restricted administration & gated updates**: `boreal.admin` assigns `admin`
  to `boreal-admin`, delegating via sanitized `sudo` only to `boreal-admin-ctl`
  for allowlisted Temporal/worker unit control and updater commands without
  access to user homes, secrets, or logs (assuming no console, hypervisor, or
  unrestricted root session). `boreal-updater` syncs `/etc/nixos` to its
  upstream `https://` tracking branch and prebuilds the host generation on
  `updater update`, queues an admission-gated drain-and-switch on
  `updater schedule` once no pipelines are running, activates the exact
  prebuilt closure, fails closed on uncertainty, and never runs automatic GC.

See [architecture](docs/architecture.md) and [updater](updater/README.md).

## Framework development

```sh
nix develop
cargo test --manifest-path common/Cargo.toml --locked
cargo test --manifest-path worker/Cargo.toml --locked
cargo test --manifest-path broker/Cargo.toml --locked
cargo test --manifest-path shell/Cargo.toml --locked
cargo test --manifest-path admin/Cargo.toml --locked
cargo test --manifest-path updater/Cargo.toml --locked
nix flake check
```

For the shell's terminal interaction checks and local standalone mock preview
(used only outside SSH when `/etc/boreal` is absent):

```sh
cargo build --manifest-path shell/Cargo.toml --locked
python3 tests/shell-pty.py shell/target/debug/boreal-shell
nix run .#boreal-shell
```

## Create or mount a downstream lab

Bootstrap a standalone lab repository from Boreal's starter template, with
`boreal` prewired as a flake input and `${boreal}/worker` linked automatically
at `.boreal/boreal-worker`:

```sh
nix flake new my-lab -t github:canonical/boreal
cd my-lab
git init && git add .
nix flake check
```

Inside a Boreal checkout, downstream deployments can also live on independent
`downstreams/<name>` orphan branches mounted inside gitignored
`downstreams/<name>/`:

```sh
# Start a new downstream orphan worktree from the local starter template:
git worktree add --orphan -b downstreams/my-lab downstreams/my-lab
(cd downstreams/my-lab && nix flake init -t ../.. && git add .)

# Or mount the reference downstreams/demo-lab orphan branch:
git worktree add downstreams/demo-lab downstreams/demo-lab

# Check or run the downstream VM against local unpushed commits on main
# (--no-write-lock-file keeps local file:// paths out of flake.lock):
nix flake check ./downstreams/demo-lab --override-input boreal . --no-write-lock-file
nix run ./downstreams/demo-lab#boreal-vm --override-input boreal . --no-write-lock-file
```

Note that Git remotes do not enforce per-branch read permissions: keep public
examples (like `downstreams/demo-lab`) on `origin`, and push private labs to
separate private Git remotes.

## Flake outputs

- `templates.default` — starter downstream lab flake
  (`nix flake new my-lab -t github:canonical/boreal`).
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
    `boreal.users`, `boreal.admin`, and `boreal.updater`.
  - `vm` — disposable QEMU guest configuration (`boreal.vm.*`).
- `packages.<system>`: `boreal-broker`, `boreal-shell`, `boreal-admin`, and
  `boreal-updater`.

See [host configuration](docs/host.md), [VM guide](docs/vm.md), and
[worker runtime](worker/README.md) for downstream usage details.

Users provision their own secrets outside Git and the Nix store under
`~/.config/boreal/secrets` (`0700` directory, `0600` files). Never place secret
contents into Temporal inputs, results, errors, or logs. Agent instructions are
in `AGENTS.md` and `.agents/skills/`.
