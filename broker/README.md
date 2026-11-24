# Boreal broker

## Remote worker ingress

When `BOREAL_REMOTE_WORKERS_CONFIG` is unset, the broker does not listen for
remote workers. When set, it names an absolute path to the deployment-managed
JSON policy. Its fields are `listen` (IP and port), `certificate` (server PEM
chain, leaf first), `privateKey` (server PEM key), `clientCa` (PEM trust anchors),
and `replicas` (objects with `owner`, `worker`, and `certificate`, the latter
pointing to exactly one PEM leaf certificate). Credential paths must be
absolute and under administrator control. Policy and credentials must be owned
by root and not writable by group or others; the private key must not be
accessible by group or others. Change credentials by restarting the broker
with the new files, not by modifying certificate contents in place.

The host listener is HTTP/2 over mutual TLS only. Every connection must present
a valid client certificate chain and an exact configured leaf. The leaf binds
the connection to that worker's catalog namespace and task queue; the listener
does not provide either of the broker's Unix-socket control interfaces. Normal
worker-gate policy still applies. Remote Nexus polling (including worker
control queues), eager task delivery, and activity completion by ID are not
supported. Use task-token completions for remote activities.

### Updates and failures

Remote workers can poll while normal drain checks report active work. The first
otherwise-quiescent check fences remote pickup and reports busy. The broker
keeps accepted polls accounted for until they settle, then runs the full drain
checks again before reporting quiescence. The fence is not lifted until the
updater reopens the gate. On restart, any recovered closed gate starts fenced,
including a gate that had not yet reached candidate quiescence. An interrupted
drain may therefore require cancelling/reopening it before retrying.

Before returning work to a remote caller, the broker stores a durable lease
with a digest of the Temporal task token, not the token itself. Remote leases
do not expire on a timer: a workflow closing in Temporal, a disconnected
replica, or a broker restart does not prove that remote code has stopped. A
token-scoped terminal worker response and a definitive Temporal result settle
the lease. Losing a worker or crashing just after persisting a grant but before
delivery can leave an orphaned lease, which blocks updates. There is no
automatic override or cleanup for that uncertainty. Stop and investigate the
affected replica before attempting administrator recovery. The fence covers
pickup through this broker; it cannot protect an independent route to Temporal
or attest to what a compromised remote host executes.

Run the TLS handshake test with OpenSSL on `PATH`:

```sh
cargo test --manifest-path broker/Cargo.toml --locked -- --include-ignored
```
