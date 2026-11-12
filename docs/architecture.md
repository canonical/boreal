# Proof-of-concept boundaries

The Temporal development server and workers demonstrate workflow execution, not
multi-user security. Temporal namespaces and task queue names are routing tools;
they do not authorize users by themselves. A local development server accepts
requests from other local users unless access is separately controlled.

A lab deployment requires authenticated Temporal clients, server-side
authorization that isolates users and workers, and a production Temporal
service. SSH account ownership alone does not establish a Temporal identity.
User-managed secrets belong outside Git and the Nix store. Workflow inputs,
results, errors, and heartbeat details must not contain secrets.

Workers are separate packages and processes. A process runs with its user's
Unix permissions; users with SSH access control their own account, while the
administrator can access all accounts and secrets. Workflow code is replayed
from event history after worker restarts. Temporal replay cannot be disabled.
Activities can be retried after interruptions, so their effects must be safe to
repeat.

The scheduled updater must not run a system switch merely because a Temporal
list or count query reports zero: Visibility is eventually consistent, and a
new workflow can start concurrently. Safe automatic switching requires an
enforced admission barrier, tracked and reconciled work, coordinated schedules,
and tests for outages and interrupted updates. Until those exist, the updater
must fail closed; this proof of concept does not assert that a host is idle.
