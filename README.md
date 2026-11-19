# Boreal `downstreams/demo-lab` deployment

Reference downstream lab consuming `github:canonical/boreal`. This orphan
branch (`downstreams/demo-lab`) lives in gitignored `downstreams/demo-lab`
inside a Boreal checkout. `boreal-vm` is the central QEMU guest and starts
`compute-01` as a NixOS container. Both poll `alice-greet` in the same Temporal
namespace (`alice`); all other workers, Temporal, PostgreSQL, the Boreal SSH
interfaces, and the updater stay on `boreal-vm`.
The container shares the VM kernel: this exercises remote worker transport,
not isolation from the VM administrator.

The broker's remote worker listener is `192.0.2.1:7443` on `ve-compute-01`,
reachable from the container at `192.0.2.2`. The veth has peer-only routes;
there is no NAT or forwarded port for the container. At boot the VM generates
a demo CA, an IP-SAN server certificate, and a pinned client certificate for
`alice-greet@compute-01`. The private keys are created with mode `0600` in
`/run`, not in the Nix store. Only the public CA certificate and the client
certificate/key are mounted into the container, read-only. The CA signing key
is deleted after issuance. This PKI is only for the disposable demo, not a
production provisioning example. The certificates are valid for seven days;
reboot the demo VM to generate a new set.

Note: `downstreams/demo-lab` shares a disposable demo SSH public key across
`alice`, `bob`, and `admin` for local VM testing; replace those keys with
distinct per-user public keys in any real deployment.

## Development and checks

Run these commands from `downstreams/demo-lab`. The existing lock pins an older
local Boreal revision, so override it with a checkout containing remote worker
support. Refresh the lock against a suitable published revision before using
it on another machine; the commands here do not update it.

```sh
demo_flake=.
boreal_input=../..
```

If either checkout has new, untracked source files, use snapshots instead:
Git-backed flakes omit those files. Review the untracked files before taking a
snapshot; do not include credentials. `git ls-files` excludes ignored files
such as `.boreal`, build outputs, and qcow2 images.

```sh
snapshot=$(mktemp -d)
snapshot_sources() {
  mkdir -p "$2"
  git -C "$1" ls-files -z --cached --others --exclude-standard |
    tar -C "$1" --null --no-recursion -cf - -T - |
    tar -xf - -C "$2"
}
snapshot_sources ../.. "$snapshot/boreal"
snapshot_sources . "$snapshot/demo"
demo_flake="path:$snapshot/demo"
boreal_input="path:$snapshot/boreal"
```

The `fleet` check validates both evaluated NixOS hosts and their placements;
the `lab` check validates the central lab's generated schemas and policies.

```sh
nix flake check "$demo_flake" --override-input boreal "$boreal_input" --no-write-lock-file
nix develop "$demo_flake" --override-input boreal "$boreal_input" --no-write-lock-file
cargo test --manifest-path temporal/Cargo.toml --workspace --locked
```

## Run the disposable VM

Choose a fresh disk path so no existing qcow2 is reused or overwritten. If
another VM already uses SSH port `2222`, shut it down before launching. The
`QEMU_KERNEL_PARAMS` line opens an unauthenticated root shell **only on this
VM's local serial console** for smoke checks. Omit that line for ordinary SSH
use, and don't enable it on a shared or production VM.

```sh
disk_dir=$(mktemp -d "$HOME/boreal-replica-demo.XXXXXXXX")
printf 'Fresh VM disk directory: %s\n' "$disk_dir"
NIX_DISK_IMAGE="$disk_dir/boreal-vm.qcow2" \
  QEMU_KERNEL_PARAMS='systemd.debug_shell=ttyS0 systemd.mask=serial-getty@ttyS0.service' \
  nix run "$demo_flake#boreal-vm" --override-input boreal "$boreal_input" --no-write-lock-file
```

In another terminal, set `disk_dir` to the printed directory to keep the fresh
VM's SSH host key separate from other VMs. Connect as `alice` or `bob` with the
private key matching the demo public key:

```sh
vm_ssh() { ssh -p 2222 -o "UserKnownHostsFile=$disk_dir/known_hosts" "$@"; }
vm_ssh alice@127.0.0.1
# Inside boreal-shell:
workers
permissions
run greet alice-run-1 'Alice Smith' 5
status alice-run-1
result alice-run-1
run count alice-run-2 'Élan' 1
secrets set api_token
secrets list
exit
```

Or pipe a secret over stdin without putting secret contents in arguments or
history:

```sh
vm_ssh alice@127.0.0.1 'secrets set api_token' < "$HOME/private/my-token-file"
```

## Verify the peripheral replica

The greeting includes the hostname of the worker that ran the activity:
`Hello Replica from boreal-vm.` or `Hello Replica from compute-01.` A fresh
workflow result is enough to identify the activity host without root access.

From the VM's local root console, check service status and the veth:

```sh
systemctl status boreal-demo-pki container@compute-01 boreal-broker boreal-worker-alice-greet
ip -4 address show dev ve-compute-01
ss -lnt 'sport = :7443'
nixos-container run compute-01 -- systemctl status boreal-worker-alice-greet
```

Query Temporal from its isolated namespace (not through the broker ingress).
After the workers have polled, the `alice-greet` queue should list identities
for both `boreal-vm` and `compute-01`:

```sh
ip netns exec boreal-internal temporal task-queue describe \
  --address 127.0.0.1:7233 --namespace alice --task-queue alice-greet \
  --task-queue-type workflow --select-unversioned
```

For a stronger smoke check on the **fresh demo VM only**, stop the central
`alice-greet` replica from the VM console:

```sh
systemctl stop boreal-worker-alice-greet.service
```

From the SSH terminal, start a new workflow and check its result after it
completes. This run should return `Hello Replica from compute-01.`:

```sh
vm_ssh alice@127.0.0.1 'run greet --id replica-smoke Replica 1'
vm_ssh alice@127.0.0.1 'result replica-smoke'
```

Back on the VM console, check the workflow history for `compute-01` and restore
the central replica:

```sh
ip netns exec boreal-internal temporal workflow show \
  --address 127.0.0.1:7233 --namespace alice --workflow-id replica-smoke --output json
systemctl start boreal-worker-alice-greet.service
```

For startup issues, check `journalctl -b -u boreal-demo-pki -u
container@compute-01 -u boreal-broker` on the VM, or `nixos-container run
compute-01 -- journalctl -b -u boreal-worker-alice-greet` for the worker. Use a
new workflow ID if repeating the test on the same VM.

Connect to the restricted admin interface (`boreal-admin`) to inspect the
central services or shut down the VM when finished:

```sh
vm_ssh admin@127.0.0.1
# Inside boreal-admin:
services
service status temporal
service restart boreal-worker-alice-greet
updater status
poweroff
```
