# Agent guide

## Before editing

- Start with `README.md`, then read the relevant guide, implementation, and
  tests. Check commands and interfaces against the code; do not invent them.
- Work on the right layer. `main` contains the reusable framework. Lab users,
  SSH keys, worker registrations, and lab policy belong in downstream flakes:
  standalone repos or orphan-branch worktrees under gitignored `downstreams/`.
  Check which checkout you are in before editing.

| Area | Start here |
| --- | --- |
| Flake API, packaging, NixOS services | `flake.nix`, `lib/`, `packages/`, `modules/` |
| Downstream SDK, private schema renderer, and starter | `sdk/`, `sdk/catalog/`, `templates/default/` |
| Shared catalogs, schemas, and IPC | `common/` |
| SSH shell, authorization, admin, and activation | `shell/`, `broker/`, `admin/`, `updater/` |
| Module, terminal, and VM checks | `tests/`, `CONTRIBUTING.md` |

## Engineering decisions

- **Keep Boreal thin.** Prefer standard NixOS deployment mechanisms and stock
  Temporal APIs. Boreal owns packaging, worker wiring, and lab access policy,
  not a new workflow authoring model. Preserve Temporal determinism and replay
  compatibility when changing downstream workflows. Keep the NixOS requirement
  on deployment hosts, not on local development machines.
- **Start concrete.** Use explicit enums and exhaustive matches for state and
  policy. Add a trait or generic only when there is a concrete need. Keep Rust
  modules flat, validate invariants at boundaries, and use borrowed views where
  ownership is unnecessary. Use the standard library first; justify new
  dependencies and keep async runtimes at SDK and server boundaries that need
  them. Preserve the existing Cargo layout and per-crate lockfiles.
- **Enforce policy at the trusted boundary.** Shell validation and completions
  are for usability, not authorization. Use authenticated peer or worker
  identity, not caller-supplied identity. Do not assume a Temporal namespace
  isolates users by itself. Read `docs/architecture.md` and the real host and
  server configuration before changing or claiming an isolation guarantee.
  Treat host administrators with root access as trusted; the restricted SSH
  admin account is not unrestricted root.
- **Keep builds pinned and activation explicit.** Change lockfiles deliberately;
  keep local overrides out of deployment locks. Preserve the updater's drain,
  final quiescence check, and activation of the exact prebuilt closure. Fail
  closed when authorization or safe activation is uncertain.
- **Treat interfaces as contracts.** Check downstream callers and fixtures
  before changing CLI behavior, Nix options or outputs, generated schemas, or
  worker interfaces. Update the relevant guide and test the behavior where it
  is enforced. Prefer focused tests for state transitions and denial paths to
  tests of thin wrappers. Run the relevant checks in `CONTRIBUTING.md` and
  report what ran and what could not run.

## Live systems and confidential data

- Confirm before modifying or cancelling live lab work, changing secrets, or
  taking destructive actions. Do not run an unsafe NixOS rebuild or automatic
  garbage collection. Use disposable environments and synthetic fixtures for
  verification.
- User secrets live under `~/.config/boreal/secrets` (directory `0700`, files
  `0600`). Never read secret contents or tokens into agent context or disclose
  them to a provider. Never put secrets in Git, the Nix store, command-line
  arguments, logs, or Temporal inputs, results, errors, or heartbeats.
- Treat user data, logs, and prompts as potentially confidential. Do not share
  another user's data, including with an external AI provider, without
  authorization. Agent guidance and optional skills do not grant permissions.
