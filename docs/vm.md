# Disposable Boreal VM

This QEMU VM is a development environment, not a secure shared lab. It runs an
unauthenticated Temporal development server; Alice and Bob can access each
other's Temporal namespaces. Alice, Bob, and the passwordless-sudo `admin`
account all authorize the same public key. Anyone with its private key can
therefore administer the VM. Use only dummy data and trusted test users.

The VM configuration is separate from the host configuration. Building or
starting it does not switch `ziczac` or change `/etc/nixos`. It requires an
`x86_64-linux` host with QEMU/KVM support, enough disk space for Nix builds and
the guest disk, and an available host port 2222. The first build may download
and compile substantial dependencies.

## Build and start

From the repository root:

```sh
nix build --out-link result-boreal-vm \
  .#nixosConfigurations.boreal-vm.config.system.build.vm
VM=$(readlink -f result-boreal-vm)
mkdir -p "$HOME/boreal-vm"
cd "$HOME/boreal-vm"
NIX_DISK_IMAGE="$PWD/boreal-vm.qcow2" "$VM/bin/run-boreal-vm-vm"
```

The launcher creates the disk if it is absent and reuses it on subsequent
starts. The disk contains the Temporal development database and VM state; it
is not removed automatically. The launcher creates a separate store image at
each start instead of mounting the host's Nix store. The VM also has normal
QEMU user-mode outbound networking. The guest is configured with 4 vCPUs,
6 GiB of RAM, and a 16 GiB virtual disk. Its configuration cannot be used to
switch the host system.

SSH is forwarded to the VM only on host loopback, `127.0.0.1:2222`. Other
local host users can reach that port, so SSH authentication is still required.
The VM configuration contains the public key selected for this test; it is
not a private key. Replace it in `hosts/boreal-vm.nix` if using another key.

## Exercise a workflow

In another terminal on the host:

```sh
ssh -i ~/.ssh/self -p 2222 alice@127.0.0.1
```

Inside the VM, check the services and start a greeting workflow:

```sh
systemctl status temporal-dev boreal-worker-alice-greet
export TEMPORAL_NAMESPACE=alice
export LAB_TASK_QUEUE=alice-greet
lab-client start greet alice-demo-1 Example 5
lab-client status alice-demo-1
```

Check status again after the timer expires. Use a new workflow ID for each
start. To test Bob, SSH as `bob`, set `TEMPORAL_NAMESPACE=bob` and
`LAB_TASK_QUEUE=bob-greet`, and use a different workflow ID. For the counting
workload, use `alice-count` or `bob-count` as the matching queue and replace
`greet` with `count`. Never put differently registered worker packages on the
same queue.

To inspect or test the updater, SSH as `admin` using the same key and use
`sudo`. The updater only schedules and builds; it intentionally cannot switch
configurations. See [the updater documentation](../updater/README.md). To shut
down the VM cleanly after test work has finished:

```sh
ssh -i ~/.ssh/self -p 2222 admin@127.0.0.1 'sudo systemctl poweroff'
```

Stopping or rebuilding the VM can interrupt test work; coordinate with anyone
using it first. Do not use the development server or this updater as a
production deployment.
