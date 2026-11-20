# Central vault activities

Each opted-in user has one activity-only `<user>-vault` queue on central.
Ordinary replicas still share their original queues. Workflows schedule named
operations on the vault and receive only non-secret results. Neither the
broker nor Temporal reads credentials. There is deliberately no get-secret
operation.

The deployer grants operations by worker name. The gate checks the authenticated
worker binding, destination, operation and delivered workflow-task token before
forwarding a vault schedule. Eager delivery to the scheduling worker is disabled.
The vault gate accepts only activity worker RPCs, and cannot be enrolled remotely
or launched as a workflow. An allowed worker (including a compromised replica)
can invoke its allowed operations with arbitrary inputs: handlers must validate
inputs, fix the credential and destination server-side, sanitize failures, and
make side effects safe to retry. Grants belong to worker identities, not to
individual exported workflows or the SSH caller that launched an execution.

The vault package alone enables `boreal-worker/read-secrets` in deployed Boreal
builds. Cargo features are additive, so packages are checked and built separately.
This is a build check, not filesystem isolation. On central, ordinary workers
and schedules belonging to a vault-enabled user run under separate systemd
identities with homes hidden. The vault runs as the owner in its own network
namespace, reading their existing SSH-managed secret store. It has no default
external network access; this feature does not add egress.

The vault participates in the existing central updater shutdown. Deploy an
authorized workflow change using normal Temporal replay/versioning rules; do not
change the activity routing of an already-recorded workflow command.
Queues ending in `-vault` are reserved even when disabled. On an existing
cluster, enable the vault only on a fresh namespace or one where that queue
has never been used: pending tasks submitted before the reservation cannot
retroactively be authorized by the broker.

## Configure a vault

Build the vault as its own Cargo package with
`boreal-worker = { workspace = true, features = [ "read-secrets" ] }`. Use a
regular worker binary that registers activities only, for example by calling
`run_worker("vault", |queue| Ok(WorkerOptions::new(queue).register_activities(Vault).build()))`.
No schema binary is needed because the shell cannot launch the vault directly.

```nix
let
  vault = boreal.lib.buildRustVault pkgs {
    pname = "lab-vault";
    src = ./temporal;
    cargoLock.lockFile = ./temporal/Cargo.lock;
  };
in {
  boreal.users.alice.vault = {
    package = vault.lab-vault;
    grants.greet = [ "verify-demo-token" ];
  };
}
```

`grants.greet` allows `alice-greet` (central replica and enrolled peripherals)
to schedule exactly `verify-demo-token`. Share the activity definition with the
workflow package, not its implementation. Use `vault_activity_options` with
the workflow context's namespace to select the queue deterministically. Keep
the total activity timeout bounded so an unavailable vault does not stall the
workflow indefinitely.
