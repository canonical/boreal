# Ongoing work

## Current state

- `main` provides the complete Boreal NixOS/Temporal framework (`boreal.lib`,
  `boreal.nixosModules.{default,boreal,vm}`, `templates.default`,
  `boreal-common`, `boreal-worker`, `boreal-broker`, `boreal-shell`,
  `boreal-admin`, and `boreal-updater`). Concrete lab deployments live in
  standalone downstream flakes initialized from `templates.default` or on
  `downstreams/<name>` orphan branches checked out inside gitignored
  `downstreams/` (such as `downstreams/demo-lab`).
- Nix generates authoritative group-readable catalogs under `/etc/boreal/`
  (`0750`, `root:boreal`) outside writable home directories and without secrets
  in the Nix store:
  - `/etc/boreal/workers.yaml` (`0440`, `root:boreal`) — registered
    `workflowType`, `description`, and typed `args` schemas.
  - `/etc/boreal/users/<user>/perms.yaml` (`0440`, `root:boreal`) — default and
    resolved per-worker `read` (`status`, `result`, `list`) and `write` (`start`
    and `cancel`) policies.
- **Runtime permission enforcement and host isolation (`boreal-broker` &
  `modules/{temporal,boreal}.nix`)**:
  - Production `services.temporal` (`temporal-server`) and PostgreSQL run inside
    `/run/netns/boreal-internal` (`0770 postgres:boreal-pg` Unix socket),
    unreachable from user SSH sessions.
  - `boreal-broker` authenticates callers on `/run/boreal/broker.sock` via
    kernel `SO_PEERCRED`, validates typed workflow arguments, enforces
    `/etc/boreal/` `read`/`write` policies (including delegated access), and
    verifies registered `workflowType` and pinned `run_id` on existing runs.
  - Each `boreal-worker-<user>-<worker>.service` runs in
    `/run/netns/boreal-worker-<user>-<worker>` with `ProtectProc = "invisible"`
    and `kernel.yama.ptrace_scope = 2`, reaching Temporal only through a
    per-worker HTTP/2 gRPC gate restricting RPC methods, `namespace`, and
    `task_queue`.
- **Data-driven `boreal-shell` and user secrets (`shell/` & `worker/`)**:
  - `boreal-shell` renders a two-line Starship-style status prompt
    (`<user> on 󰇅 <host>` or `<user> on 󰇅 mock`, caller-scoped
    `[ <paused>  <running>]` workflow counts, `is ❄️ v<version>` from
    `temporal/Cargo.toml` / `/etc/boreal/version`, line-2 red `󰞴` drain badge,
    and status/modal `❯`/`❮` indicators), dynamically builds `clap`/`reedline`
    commands and permission-filtered completions from `/etc/boreal/` (falling
    back to a local mock preview only outside SSH when `/etc/boreal` is absent),
    requires `[y/N]` confirmation before `cancel`, and rejects unsupported SSH
    commands, SFTP, and port/agent forwarding.
  - Users manage `~/.config/boreal/secrets` (`0700` directory, `0600` files) via
    echo-free TTY prompts or stdin (`ssh <user>@<host> "secrets set <name>" < file`),
    and worker activities read secrets locally via `boreal_worker::read_secret`.
- **Restricted administration and gated updates (`admin/` & `updater/`)**:
  - `boreal-admin` and `boreal-admin-ctl` restrict `admin` to allowlisted
    `temporal.service` / `boreal-worker-*` control and updater operations under
    a sanitized root environment with zero access to user homes, secrets, or
    logs (assuming no console, hypervisor, or unrestricted root session).
  - `boreal-updater` enforces the `/etc/nixos` + `networking.hostName` contract
    (`updater update` hard-resets `/etc/nixos` to its upstream `https://`
    branch, verifies `flake.lock` hygiene, and prebuilds the generation;
    `updater schedule` queues `boreal-updater.service` asynchronously),
    coordinates a durable admission barrier and
    schedule/descendant/lease/PostgreSQL `current_executions` drain protocol,
    activates the exact prebuilt closure when `system.switch.enable = true`,
    fails closed (`phase=blocked`) on uncertainty, reconciles interrupted updates
    on boot via `boreal-updater-recover.service`, and never runs automatic GC.

## Verification

- Unit tests across `common/`, `worker/`, `broker/`, `shell/`, `admin/`, and
  `updater/`.
- PTY interaction suite: `tests/shell-pty.py`.
- NixOS module and template checks: `tests/modules.nix` and `templates/default`.
- End-to-end QEMU/KVM integration suite: `tests/vm.nix` (`tests/vm-suite.py`)
  and disposable `downstreams/demo-lab#boreal-vm` verification.
