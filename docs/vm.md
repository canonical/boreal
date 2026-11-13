# Disposable Boreal VM

This QEMU VM is a development environment, not a secure shared lab. It runs an
unauthenticated Temporal development server without runtime policy enforcement;
Alice and Bob can still reach each other's Temporal namespaces directly even
though Nix generates `/etc/boreal/users/{alice,bob}/perms.yaml`. Alice, Bob, and
the passwordless-sudo `admin` account also authorize the same test public key.
Anyone with its private key can therefore administer the VM. Use only dummy data
and trusted test users.

The VM configuration is separate from the host configuration. Building or
starting it does not switch `ziczac` or change `/etc/nixos`. It requires an
`x86_64-linux` host with QEMU/KVM support, enough disk space for Nix builds and
the guest disk, and an available host port 2222. The first build may download
and compile substantial dependencies.

## Build and start

From the repository root:

```sh
mkdir -p "$HOME/boreal-vm"
NIX_DISK_IMAGE="$HOME/boreal-vm/boreal-vm.qcow2" nix run .#boreal-vm
```

Alternatively, build the launcher and run it directly:

```sh
nix build --out-link result-boreal-vm .#boreal-vm
mkdir -p "$HOME/boreal-vm"
NIX_DISK_IMAGE="$HOME/boreal-vm/boreal-vm.qcow2" \
  ./result-boreal-vm/bin/run-boreal-vm
```

The launcher creates the disk if it is absent and reuses it on subsequent
starts. Without `NIX_DISK_IMAGE`, it uses `./boreal-vm.qcow2` in the current
directory. The disk contains the Temporal development database and VM state;
it is not removed automatically. The launcher creates a separate store image at
each start instead of mounting the host's Nix store; its temporary directory
is not automatically removed by the launcher. The VM also has normal QEMU
user-mode outbound networking. The guest is configured with 4 vCPUs, 6 GiB of
RAM, and a 16 GiB virtual disk. Its configuration cannot be used to switch the
host system.

SSH is forwarded to the VM only on host loopback, `127.0.0.1:2222`. Other
local host users can reach that port, so SSH authentication is still required.
The VM configuration contains the public key selected for this test; it is
not a private key. Replace it in `hosts/boreal-vm.nix` if using another key.

## Exercise workflows and inspect generated policy

In another terminal on the host:

```sh
ssh -i ~/.ssh/self -p 2222 alice@127.0.0.1
```

Inside the VM, inspect the generated group-readable policy files, check the
worker services, and start a `greet` workflow:

```sh
cat /etc/boreal/users/alice/perms.yaml
systemctl status temporal-dev boreal-worker-alice-greet
export TEMPORAL_NAMESPACE=alice
export LAB_TASK_QUEUE=alice-greet
lab-client start greet alice-run-1 Example 5
lab-client status alice-run-1
```

Check status again after the timer expires. Use a new workflow ID for each
start. To test Bob, SSH as `bob`, set `TEMPORAL_NAMESPACE=bob` and
`LAB_TASK_QUEUE=bob-greet`, and use a different workflow ID. For the `count`
workflow, use `alice-count` or `bob-count` as the matching queue and replace
`greet` with `count`. Never put differently registered worker packages on the
same queue.

To inspect or test `boreal.updater`, SSH as `admin` using the same key and use
`sudo`. The updater only schedules and builds; it intentionally cannot switch
configurations. See [the updater documentation](../updater/README.md). To shut
down the VM cleanly after test work has finished:

```sh
ssh -i ~/.ssh/self -p 2222 admin@127.0.0.1 'sudo systemctl poweroff'
```

Stopping or rebuilding the VM can interrupt test work; coordinate with anyone
using it first. Do not use the development server or this VM configuration as a
production deployment.
