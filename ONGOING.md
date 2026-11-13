# Ongoing work

## Current state

- `boreal.enable`, `boreal.users.<user>`, and `boreal.updater` replace
  `boreal.poc`. Worker `package` names resolve against the Boreal worker
  packages (`lab-worker` and `lab-worker-alt`), preserving the `greet` and
  `count` Temporal workflow types, `<user>` namespaces, and `<user>-<worker>`
  queues.
- Nix generates authoritative group-readable policy files at
  `/etc/boreal/users/<user>/perms.yaml` (`0440`, `root:boreal` under `0750`
  directories) outside writable home directories and without secrets in the Nix
  store. A user always controls their own work; default `read` (`status` and
  `result`) and `write` (`start` and `cancel`) policies apply to all of their
  workers unless a worker-specific setting replaces that field (`true` grants
  all Boreal users, `false` grants none beyond the owner, and a list grants
  named Boreal users). **These policy files are not enforced at runtime yet.**
- The local `boreal-shell` prototype parses commands and supports completion,
  highlighting, and session history, but every workflow command is a mock. It
  is not a login shell or an authorization boundary.
- The `greet` and `count` workflows and their workers run in the disposable VM.
  The VM's Temporal development server is unauthenticated, all three accounts
  share a test SSH key, and `admin` has unrestricted passwordless sudo. This is
  not safe for a shared lab.
- The updater schedules and prebuilds revisions under `boreal.updater`, and
  activation remains blocked.

## Next steps

### 1. Enforce permissions before connecting the shell

- Use SSH and OS identity to identify the caller; never trust a caller-supplied
  username. Apply the Nix-generated `/etc/boreal/users/<user>/perms.yaml` policy
  before start, cancel, status, result, and list operations, including
  explicitly delegated access to another user's worker. Verify the actual
  workflow type and run when checking existing work.
- Keep direct Temporal endpoints, internal services, and the database out of
  users' reach. Work out and test host-level isolation for workers that need
  Temporal access while running under their users' Unix accounts. A restricted
  shell, namespace names, or loopback binding alone do not enforce this.
- Use a production-suitable Temporal service for a shared lab. The preferred
  approach is the restricted shell plus host-enforced isolation, not a new JWT
  issuer or Temporal authentication service. Do not claim isolation until direct
  access and alternative SSH paths have been tested.

### 2. Turn the mock shell into the user interface

- Connect `workers`, `start`, `list`, `status`, `result`, `cancel`, and
  `permissions` to the enforced operations. Filter listings and completions by
  permission. Require confirmation before cancelling work.
- Keep the shell-like `reedline`/`clap` interface and reject unsupported SSH
  command, subsystem, and forwarding paths rather than providing a general
  shell. Decide how users can manage their own secrets without exposing them in
  arguments, history, logs, the Nix store, or Temporal history.
- Test authorized and denied cross-user actions, direct-access bypass attempts,
  completion, terminal behavior, and workflow recovery in a disposable VM.

### 3. Restrict administration and finish safe updates

- Design the admin's restricted root interface for starting/stopping the
  permitted Temporal services and requesting Nix configuration updates. The
  intended threat model assumes no alternate admin access through a console,
  hypervisor, recovery mode, or unrestricted root session. Protect user files
  and secrets from that restricted interface; test that it cannot be bypassed.
- GitHub review is managed outside Boreal; do not add a separate signature or
  approval service. Limit updates to the intended repository and reviewed branch,
  and activate the exact revision that was prebuilt.
- Before enabling system activation, implement an enforced admission barrier
  and authoritative drain/recovery protocol covering concurrent starts,
  schedules, child workflows, activities, failures, and interrupted updates.
  Keep activation blocked when safety is uncertain; do not enable automatic
  garbage collection.

Develop and test the VM inside this repository. Do not rebuild or switch the host
without separate confirmation.
