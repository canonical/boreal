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
  `namespace` (`<user>`), and `task_queue` (`<user>-<worker>`). A package
  `schedule` output is started as a oneshot in that namespace and may upsert
  Temporal schedules through the same gate.
- **Kernel-authenticated runtime policy**: `boreal-broker` authenticates callers
  on `/run/boreal/broker.sock` (`0660 boreal-broker:boreal`) via `SO_PEERCRED`,
  validates workflow input against `/etc/boreal/schemas.yaml` schema
  directories, enforces `/etc/boreal/users/<user>/perms.yaml` across `run`,
  `cancel`, `status`, `result`, `view`, `list`, and `permissions`, and verifies
  the registered `workflow` and pinned `run_id` on existing executions.
- **Restricted SSH shell & user secrets**: OpenSSH disables SFTP, TCP/Unix/agent
  forwarding, tunnels, and `~/.ssh/rc`. Both shells share a two-line prompt
  (`<user> on 󰇅 <host> is ❄️ <deployment>`) and modal `❯`/`❮` indicators.
  The deployment label is the downstream Git hash or an exact version tag.
  `boreal-shell` also shows workflow counts and a red `󰞴` drain badge, and
  drives commands, live `view` TUI inspection, and
  permission-filtered completions from `/etc/boreal/`. The private
  `~/boreal.history` keeps up to 2,000 interactive command selectors, never
  workflow inputs or scripted invocations. Arrow Up recalls a `run` line up to
  16 KiB exactly in the same session; after reconnecting only selectors remain.
  The shell requires `[y/N]` confirmation before `cancel`, allows
  non-interactive `run`, `list`, `status`,
  and `result`, and stores secrets under `~/.config/boreal/secrets` (`0700`/`0600`)
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

See [architecture](docs/architecture.md), [central and peripheral workers](docs/remote-workers.md),
[central vault activities](docs/vault.md), and [updater](updater/README.md).

## Label a deployment with its Git version tag

Both SSH shells show the deployed **downstream** revision, not the revision of
the Boreal framework input. Use the updater for production updates or build
from a downstream Git checkout with `boreal-build`:

```sh
nix run github:canonical/boreal#boreal-build -- . lab-01
# Use `vm` as the third argument to build a configured VM instead:
nix run github:canonical/boreal#boreal-build -- . boreal-vm vm
```

`boreal-build` only builds and creates a `result` link; it does not activate the
generation. The label uses a version tag such as `v1.2.3` on the exact built
commit, including annotated tags. If several version tags match, it uses the
first in lexical order. Otherwise it shows the seven-character commit hash.
A downstream checkout with staged or unstaged tracked changes shows `<hash>+`,
even if its `HEAD` is tagged. Untracked files are not included in Git flake
builds.

Plain flake builds can use revision metadata when the downstream passes
`specialArgs = { inherit self; };`, but do not see Git tags and may lose the
revision when the effective lock changes. Use `boreal-build … --override-input
boreal FLAKE` to keep the downstream label with a local framework override,
without rewriting the lockfile. Sources without revision metadata show
`unknown`.

## Run a workflow

In the interactive lab shell, `run greet` opens a form for missing input.
Press Enter to accept a dimmed default. In choice lists, Up/Down moves the
selection and Left/Right changes pages (up to five items per page), keeping
the same row when possible. Press Esc to edit a previous answer. Ctrl+C
abandons the form; nothing starts until you confirm the summary. Field help
comes from the Rust input type's doc comments. If the form cannot represent an
input shape, use `--input-json` instead.

`run greet --id greet-1 --input-json '{"name":"Alice","wait_seconds":5}'`
starts directly. The worker is the only positional argument. Use `--no-ask`
to reject missing values rather than opening a form. For a worker
with multiple exports, select an alias with `--workflow` or choose it in the
form. If `--id` is omitted, Boreal generates a workflow ID.

For scripts, use a non-interactive SSH command. It never prompts:

```sh
ssh -T alice@lab-infra.local \
  'run greet --id greet-1 --input-json -' < input.json
```

`--input-json -` reads JSON from stdin. Supplying an inline JSON argument also
works if the remote command preserves its quoting. Success is printed on
stdout; errors go to stderr with a nonzero exit status. Don't put secrets in
workflow inputs.

## Run a lab locally

In a downstream checkout wired for [`boreal-local`](docs/local.md):

```sh
nix develop -c boreal-local
nix develop -c boreal-local run example --id local-1 --input-json - < input.json
nix develop -c boreal-local result local-1
nix develop -c boreal-local stop
```

The interactive command opens the real Boreal shell. Rootless Podman runs
Temporal and the worker containers on this machine, without outside network
access or a VM. The local profile keeps execution state, optional vault
secrets, and selector-only shell history across sessions and builds. Use a
VM to verify production deployment and authorization behavior.

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
nix run .#boreal-build -- ./downstreams/demo-lab boreal-vm vm --override-input boreal .
./result/bin/run-boreal-vm
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
  - `mkFleetCheck { pkgs, hosts }` — checks that the specified NixOS hosts
    share a worker placement map and have exactly one central host.
   - `mkDevShell pkgs { ... }` and `buildRustWorker pkgs { ... }` — Rust
     development shell and Temporal worker package builder linking
     `${boreal}/worker` at `.boreal/boreal-worker` without manual `outputHashes`.
   - `mkLocal pkgs { workers, vault ? null }` — a `boreal-local` launcher
     for a single-user local lab.
- `nixosModules`:
  - `default` / `boreal` — `boreal.enable`, `boreal.users`, `boreal.admin`,
    and `boreal.updater`.
  - `vm` — disposable QEMU guest configuration (`boreal.vm.*`).
- `packages.<system>`: `boreal-broker`, `boreal-shell`, `boreal-admin`,
  `boreal-build`, and `boreal-updater`.

See [host configuration](docs/host.md), [VM guide](docs/vm.md), and
[worker runtime](worker/README.md) for downstream usage details.

Users provision their own secrets outside Git and the Nix store under
`~/.config/boreal/secrets` (`0700` directory, `0600` files). Never place secret
contents into Temporal inputs, results, errors, or logs. Agent instructions are
in `AGENTS.md` and `.agents/skills/`.
