# Architecture and security boundaries

Boreal separates reusable lab infrastructure (on `main`) from concrete lab
deployments (in standalone downstream flakes initialized from
`templates.default` or on `downstreams/<name>` orphan-branch worktrees under
`downstreams/`). Downstream labs pin `boreal` in `flake.lock` and declare accounts, SSH public
keys, worker outputs, schedules, and permissions in `boreal.users`. Each worker
package publishes `schema.yaml` from a build-time `schema` bin.

## Policy catalogs

Nix generates protected, group-readable catalogs under `/etc/boreal/` (`0750`,
`root:boreal`) outside writable home directories and without secrets in the Nix
store:

- `/etc/boreal/version` (`0440`, `root:boreal`): downstream lab version
  (`boreal.version`, defaulting to `temporal/Cargo.toml`
  `workspace.package.version`).
- `/etc/boreal/schemas.yaml` (`0440`, `root:boreal`): store directories that
  contain `schema.yaml`. Each file is keyed by worker output name and carries
  `workflow`, `description`, and ordered `args`.
- `/etc/boreal/users/<user>/perms.yaml` (`0440`, `root:boreal`): each user's
  default and resolved per-worker `read` (`status`, `result`, `view`, `list`)
  and `write` (`run`, `cancel`) policies (`true` grants all Boreal users,
  `false` grants none beyond the owner, and a list grants named Boreal users;
  owners always control their own workers).

## Host and network-namespace isolation

- **Internal control plane (`/run/netns/boreal-internal`)**: Production
  `services.temporal` (`temporal-server`) and PostgreSQL run inside
  `boreal-internal`. PostgreSQL's Unix socket (`/run/postgresql/.s.PGSQL.5432`)
  is `0770 postgres:boreal-pg`, accessible only to `postgres`, `temporal`, and
  `boreal-broker`. Host-namespace user accounts cannot connect to Temporal or
  PostgreSQL over TCP or Unix sockets.
- **Policy broker (`boreal-broker.service`)**: Before entering `boreal-internal`
  and dropping privileges to `boreal-broker` (with `PR_SET_DUMPABLE = 0`),
  `boreal-broker` binds:
  - `/run/boreal/broker.sock` (`0660 boreal-broker:boreal`) for `boreal-shell`.
    Every request is authenticated using kernel `SO_PEERCRED` (`UID -> passwd`),
    never a caller-supplied username. Before acting on an existing execution,
    `boreal-broker` describes the workflow, resolves its task queue back to the
    configured worker, verifies `workflow` matches the registered schema,
    and pins `run_id` across `cancel`.
  - `/run/boreal/updater.sock` (`0600 root:root`, verified `SO_PEERCRED` UID `0`)
    for `boreal-updater`.
  - Per-worker loopback listeners inside each worker network namespace.
- **Worker isolation (`/run/netns/boreal-worker-<user>-<worker>`)**: Each worker
  runs as `<user>` with `NoNewPrivileges = true`, `PrivateTmp = true`,
  `ProtectSystem = "strict"`, `ProtectProc = "invisible"`, and host
  `kernel.yama.ptrace_scope = 2` (blocking same-UID `ptrace` from interactive
  sessions). `boreal-broker`'s HTTP/2 gRPC worker gate permits worker
  polling/completion RPCs on `WorkflowService`, plus `StartWorkflowExecution`,
  `GetWorkflowExecutionHistory`, `RequestCancelWorkflowExecution`, and the
  schedule RPCs (`CreateSchedule`, `DescribeSchedule`, `UpdateSchedule`,
  `PatchSchedule`, `DeleteSchedule`, `ListSchedules`, `CountSchedules`,
  `ListScheduleMatchingTimes`) when the target namespace, task queue, and
  `workflow` match a registered worker and
  `/etc/boreal/users/<owner>/perms.yaml` grants the listener owner `write`
  (start, cancel, and schedule mutation) or `read` (history and cross-namespace
  describe). List, count, and matching-times stay in the listener owner's
  namespace. Caller identity is the per-worker listener binding, never the
  `temporal-namespace` header or request namespace.
  Cross-namespace child-workflow commands, external signals, and external
  cancels stay denied. Permitted starts are recorded for updater drain; a closed
  admission gate still accepts one only when that caller namespace already has
  in-flight tracked work or a live worker lease. An unpaused schedule create or
  update, an unpause, a trigger, and a backfill are rejected while the gate is
  closed, with no inflight exception. Pause, delete, and a paused create stay
  allowed. Create and update clamp `catchup_window` to 60 seconds, and a
  backfill wider than 24 hours is rejected. Polling RPCs still require
  `namespace == <user>` and `task_queue == <user>-<worker>`.

## SSH surface and user secrets

OpenSSH disables root login, SFTP, TCP/Unix/agent/X11 forwarding, tunnels,
`~/.ssh/rc`, and `~/.ssh/environment`. Lab users have `0700` home directories
and login shell `boreal-shell`. Interactive sessions read `~/.borealrc` (`0600`
regular file, `O_NOFOLLOW`, provisioned from `boreal.users.<name>.editMode` as
`emacs`, `vi`, or `helix` with `emacs` fallback), render a two-line
Starship-style prompt (`<user> on 󰇅 <host> [ <paused>  <running>] is ❄️ v<version>`,
with a line-2 red `󰞴` drain badge and modal `❯`/`❮` indicators), and expose
dynamic `clap` / `reedline` commands (`workers`, `run`, `list`, `status`,
`result`, `view`, `cancel`, `permissions`, `secrets`, `help`, `exit`), an
alternate-screen `view` TUI for live step/event inspection, and
permission-filtered completions; non-interactive SSH accepts only
`secrets set <name>`.

Secrets live in `~/.config/boreal/secrets/<name>` (`0700` directory, `0600`
regular files, `O_NOFOLLOW`) and are written via non-echo TTY prompt or stdin
redirection (`ssh <user>@<host> "secrets set <name>" < file`). Worker activities
read secrets locally via `boreal_worker::read_secret` / `read_secret_bytes`.
Workflow inputs, results, errors, heartbeats, and logs must never contain secret
values.

## Restricted administration and gated updates

`boreal.admin` provisions `admin` with login shell `boreal-admin` and a narrow
`NOPASSWD` `sudo` rule permitting only `/nix/store/.../bin/boreal-admin-ctl`.
`boreal-admin-ctl` clears its environment, sets `umask 077` and `chdir /`, and
permits only status/start/stop/restart on `temporal.service` and configured
`boreal-worker-*` units, zero-argument `updater` subcommands (`update`,
`schedule`, `status`, `recover`), and optional VM `poweroff`. It never reads
user home directories, secrets, or service logs. This model assumes no alternate
console, hypervisor, recovery-mode, or unrestricted root session.

`boreal-updater` reads `/etc/boreal/updater.yaml` (`0400 root:root`) and
enforces the `/etc/nixos` downstream checkout contract: `update` verifies
`/etc/nixos` root ownership, fetches and hard-resets to its upstream `https://`
tracking branch, rejects local `file://` or `path` overrides in committed
`flake.lock`, and prebuilds `nixosConfigurations.<networking.hostName>` to a GC
root; `schedule` queues `boreal-updater.service` asynchronously. Before
switching, `boreal-updater` closes the durable admission gate in
`boreal-broker`, pauses unpaused Temporal schedules, drains all tracked
executions, schedule runs, child/continue-as-new descendants, worker task
leases, and PostgreSQL `current_executions` rows, stops worker units, rechecks
quiescence, and activates the exact prebuilt store closure. Uncertain states
remain `blocked` with no automatic garbage collection.
