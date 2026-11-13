# Architecture and security boundaries

Nix generates per-user access policies at `/etc/boreal/users/<user>/perms.yaml`
(`0440`, `root:boreal` under `0750` directories) so a writable home directory
cannot replace or shadow the policy, and policy files in the Nix store contain
no secrets. However, **runtime policy enforcement and Temporal isolation are not
yet wired up**: a local development server still accepts requests from local
users unless host and server controls restrict access. Temporal namespaces and
task queue names are routing tools; they do not authorize users by themselves.

A shared-lab deployment requires host-enforced isolation and an enforced policy
gate in front of Temporal operations, backed by a production-suitable Temporal
service. SSH account ownership alone does not establish a Temporal identity.
User-managed secrets belong outside Git and the Nix store. Workflow inputs,
results, errors, and heartbeat details must not contain secrets.

`greet` (`DemoWorkflow` / `greet`) and `count` (`CountWorkflow` / `count_chars`)
are Boreal workflows run by separate worker packages and processes
(`lab-worker` and `lab-worker-alt`). Their Temporal workflow types, `<user>`
namespaces, and `<user>-<worker>` task queues are preserved so existing workflow
histories remain replay-compatible across migrations. A worker runs with its
user's Unix permissions; users with SSH access control their own account, while
a trusted administrator can access accounts and secrets unless restricted.
Workflow code is replayed from event history after worker restarts, and Temporal
replay cannot be disabled. Activities can be retried after interruptions, so
their effects must be safe to repeat.

The scheduled updater must not run a system switch merely because a Temporal
list or count query reports zero: Visibility is eventually consistent, and a
new workflow can start concurrently. Safe automatic switching requires an
enforced admission barrier, tracked and reconciled work, coordinated schedules,
and tests for outages and interrupted updates. Until those exist, the updater
must fail closed and does not assert that a host is idle.
