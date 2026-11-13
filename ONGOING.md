# Ongoing work

## Current state

- The local `boreal-shell` prototype parses commands and supports completion,
  highlighting, and session history, but every workflow command is a mock. It
  is not a login shell or an authorization boundary.
- The `greet` and `count` workflows and their workers run in the disposable VM.
  The VM's Temporal development server is unauthenticated, all three accounts
  share a test SSH key, and `admin` has unrestricted passwordless sudo. This is
  not safe for a shared lab.
- The updater schedules and prebuilds revisions, but activation is blocked.
  `boreal.poc` is still the implemented NixOS module interface.

## Next steps

### 1. Establish the Boreal module

- Replace the `boreal.poc` module definition with `boreal.enable` and nested
  `boreal.users.<user>` settings for SSH public keys, permissions, and workers.
  Resolve each worker's package name against the known Boreal packages.
- Target configuration shape (not implemented yet):

  ```nix
  boreal.enable = true;
  boreal.users.alice = {
    keys = [ "<Alice's public SSH key>" ];
    permissions = { read = true; write = false; };
    workers = {
      greet = { package = "lab-worker"; permissions.write = [ "bob" ]; };
      count = { package = "lab-worker-alt"; permissions.read = [ "bob" ]; };
    };
  };
  ```

- Generate Nix-managed per-user `perms.yaml` files. Make them group-readable;
  use a protected authoritative copy so a writable home directory cannot
  replace the policy. Policy files in the Nix store must contain no secrets.
- A user always controls their own work. Default `read` and `write` values
  apply to all their workers; a worker-specific setting replaces that field.
  `true` grants all lab users, `false` grants none, and a list grants named
  users. `read` covers status and results; `write` grants start and cancel.
  For example, Alice's `count.permissions.read = [ "bob" ]` restricts count
  results to Alice and Bob even if her default read setting is `true`.
- Treat `greet` and `count` as Boreal workflows. Preserve their Temporal workflow
  types, namespaces, queues, and replay compatibility when migrating existing
  work. Move the updater options out of `boreal.poc`, update the VM, module tests,
  and documentation, and keep activation blocked.

### 2. Enforce permissions before connecting the shell

- Use SSH and OS identity to identify the caller; never trust a caller-supplied
  username. Apply the Nix-generated policy before start, cancel, status, result,
  and list operations, including explicitly delegated access to another user's
  worker. Verify the actual workflow type and run when checking existing work.
- Keep direct Temporal endpoints, internal services, and the database out of
  users' reach. Work out and test host-level isolation for workers that need
  Temporal access while running under their users' Unix accounts. A restricted
  shell, namespace names, or loopback binding alone do not enforce this.
- Use a production-suitable Temporal service for a shared lab. The preferred
  approach is the restricted shell plus host-enforced isolation, not a new JWT
  issuer or Temporal authentication service. Do not claim isolation until direct
  access and alternative SSH paths have been tested.

### 3. Turn the mock shell into the user interface

- Connect `workers`, `start`, `list`, `status`, `result`, `cancel`, and
  `permissions` to the enforced operations. Filter listings and completions by
  permission. Require confirmation before cancelling work.
- Keep the shell-like `reedline`/`clap` interface and reject unsupported SSH
  command, subsystem, and forwarding paths rather than providing a general
  shell. Decide how users can manage their own secrets without exposing them in
  arguments, history, logs, the Nix store, or Temporal history.
- Test authorized and denied cross-user actions, direct-access bypass attempts,
  completion, terminal behavior, and workflow recovery in a disposable VM.

### 4. Restrict administration and finish safe updates

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
