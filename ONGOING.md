# Ongoing work

## Current state

- `main` is a reusable Boreal flake framework (`boreal.lib`,
  `boreal.nixosModules.{default,boreal,vm}`, `templates.default`,
  `boreal-worker`, `boreal-shell`, and `boreal-updater`). Concrete lab
  deployments live in standalone downstream flakes initialized from
  `templates.default` or on `downstreams/<name>` orphan branches checked out
  inside gitignored `downstreams/` (such as `downstreams/demo-lab`, preserving
  the `greet` and `count` workflows, `alice`/`bob` namespaces, and
  `<user>-<worker>` queues).
- Downstream labs register worker derivations and typed flat argument schemas in
  `boreal.workerPackages` and declare users, SSH keys, worker instances, and
  permissions in `boreal.users.<user>`.
- Nix generates authoritative group-readable catalogs under `/etc/boreal/`
  (`0750`, `root:boreal`) outside writable home directories and without secrets
  or store paths in the Nix store:
  - `/etc/boreal/workers.yaml` (`0440`, `root:boreal`) — registered
    `workflowType`, `description`, and typed `args` schemas.
  - `/etc/boreal/users/<user>/perms.yaml` (`0440`, `root:boreal`) — default and
    resolved per-worker `read` (`status` and `result`) and `write` (`start` and
    `cancel`) policies (`true` grants all Boreal users, `false` grants none
    beyond the owner, and a list grants named Boreal users; a user always
    controls their own work). **These catalogs are not enforced at runtime yet.**
- `boreal-shell` parses sample commands and supports completion, highlighting,
  and session history, but remains a hardcoded mock rather than loading
  `/etc/boreal/` or enforcing authorization.
- Disposable `boreal.lib.mkVm` guests run an unauthenticated `temporal-dev`
  server and `admin` has unrestricted passwordless sudo. `boreal.updater`
  schedules and prebuilds revisions, and activation remains blocked.

## Next steps

### 1. Enforce permissions before connecting the shell

- Use SSH and OS identity to identify the caller; never trust a caller-supplied
  username. Load `/etc/boreal/workers.yaml` and
  `/etc/boreal/users/<user>/perms.yaml` to validate typed workflow arguments and
  enforce policy before `start`, `cancel`, `status`, `result`, and `list`
  operations, including delegated access to another user's worker. Verify the
  actual registered `workflowType` and run when checking existing work.
- Keep direct Temporal endpoints, internal services, and the database out of
  users' reach. Work out and test host-level isolation for workers that need
  Temporal access while running under their users' Unix accounts. A restricted
  shell, namespace names, or loopback binding alone do not enforce this.
- Use a production-suitable Temporal service for a shared lab. The preferred
  approach is the restricted shell plus host-enforced isolation, not a new JWT
  issuer or Temporal authentication service. Do not claim isolation until direct
  access and alternative SSH paths have been tested.

### 2. Turn the mock shell into the data-driven user interface

- Drive `boreal-shell` commands, argument validation, and tab completions
  dynamically from `/etc/boreal/workers.yaml` and `/etc/boreal/users/*/perms.yaml`
  (falling back to a built-in mock catalog only for local standalone preview) and
  connect `workers`, `start`, `list`, `status`, `result`, `cancel`, and
  `permissions` to the enforced operations. Filter listings and completions by
  permission, and require confirmation before cancelling work.
- Keep the shell-like `reedline`/`clap` interface and reject unsupported SSH
  command, subsystem, and forwarding paths rather than providing a general
  shell. Decide how users can manage their own secrets without exposing them in
  arguments, history, logs, the Nix store, or Temporal history.
- Test authorized and denied cross-user actions, direct-access bypass attempts,
  completion, terminal behavior, and workflow recovery in the disposable
  `downstreams/demo-lab` VM.

### 3. Restrict administration and finish safe updates

- Design the admin's restricted root interface for starting/stopping the
  permitted Temporal services and requesting Nix configuration updates. The
  intended threat model assumes no alternate admin access through a console,
  hypervisor, recovery mode, or unrestricted root session. Protect user files
  and secrets from that restricted interface; test that it cannot be bypassed.
- GitHub review is managed outside Boreal; do not add a separate signature or
  approval service. Configure `boreal.updater` to limit updates to the
  downstream lab's intended repository and reviewed branch, and activate the
  exact revision that was prebuilt.
- Before enabling system activation, implement an enforced admission barrier
  and authoritative drain/recovery protocol covering concurrent starts,
  schedules, child workflows, activities, failures, and interrupted updates.
  Keep activation blocked when safety is uncertain; do not enable automatic
  garbage collection.

Develop and test the VM inside this repository. Do not rebuild or switch the host
without separate confirmation.
