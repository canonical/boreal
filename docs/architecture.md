# Architecture and security boundaries

Boreal separates reusable lab infrastructure (on `main`) from concrete lab
deployments (in downstream flakes or orphan-branch worktrees under
`downstream/`). Downstream labs register their Temporal worker derivations and
typed flat argument schemas in `boreal.workerPackages` and declare accounts, SSH
public keys, worker instances, and permissions in `boreal.users`.

Nix generates two protected, group-readable catalogs under `/etc/boreal/`
(`0750`, `root:boreal`):

- `/etc/boreal/workers.yaml` (`0440`, `root:boreal`): registered worker
  `workflowType`, `description`, and typed `args` metadata.
- `/etc/boreal/users/<user>/perms.yaml` (`0440`, `root:boreal`): each user's
  default and resolved per-worker `read` and `write` policies.

Keeping both catalogs under `/etc/boreal/` prevents writable home directories
from replacing or shadowing policy, and generated YAML files in the Nix store
must never contain secrets or SSH keys. However, **runtime policy enforcement
and Temporal isolation are not yet wired up**: a local development server still
accepts requests from local users unless host and server controls restrict
access. Temporal namespaces and task queue names are routing tools; they do not
authorize users by themselves.

A shared-lab deployment requires host-enforced isolation and an enforced policy
gate in front of Temporal operations, backed by a production-suitable Temporal
service. SSH account ownership alone does not establish a Temporal identity.
User-managed secrets belong outside Git and the Nix store. Workflow inputs,
results, errors, and heartbeat details must not contain secrets.

Each configured worker runs as `boreal-worker-<user>-<worker>.service` under
its user's Unix account, polling `<user>-<worker>` in namespace `<user>`.
Workflow code is replayed from event history after worker restarts, and Temporal
replay cannot be disabled; downstream workflow changes must preserve registered
workflow types and deterministic history compatibility when migrating existing
work. Activities can be retried after interruptions, so their effects must be
safe to repeat.

The scheduled updater must not run a system switch merely because a Temporal
list or count query reports zero: Visibility is eventually consistent, and a
new workflow can start concurrently. Safe automatic switching requires an
enforced admission barrier, tracked and reconciled work, coordinated schedules,
and tests for outages and interrupted updates. Until those exist, the updater
must fail closed and does not assert that a host is idle.
