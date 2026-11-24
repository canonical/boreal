# Local QEMU lab (`boreal.lib.mkVm`)

`boreal.nixosModules.vm` and `boreal.lib.mkVm` let any downstream lab build an
isolated QEMU guest from the same `lab.nix` used for bare-metal hosts,
including network-namespace Temporal/PostgreSQL isolation, `boreal-broker`
policy and worker-gate enforcement, `boreal-shell`, and restricted
`boreal-admin`.

When `boreal.vm.devAuthorizedKeys` is set, `admin` and all `boreal.users`
accounts authorize those developer public keys in the local VM; do not use
shared developer keys for untrusted multi-user hosts.

## Downstream flake wiring

```nix
nixosConfigurations.boreal-vm = boreal.lib.mkVm {
  system = "x86_64-linux";
  specialArgs = { inherit self; };
  modules = [
    ./lab.nix
    {
      boreal.updater.enable = true;
      boreal.vm = {
        diskImage = "./boreal-vm.qcow2";
        sshPort = 2222;
        devAuthorizedKeys = [ "ssh-ed25519 AAAAC3Nza..." ];
      };
    }
  ];
};
```

`boreal.nixosModules.vm` automatically:
- Runs isolated PostgreSQL, `temporal.service`, `boreal-temporal-namespaces`,
  `boreal-broker.service`, and per-worker network namespaces derived from
  `boreal.users`.
- Enables restricted `boreal.admin` with `allowPoweroff = true` and bounded
  worker restart limits.
- Disables `system.switch.enable` by default so the VM configuration cannot
  switch a host machine, uses an isolated Nix store image, disables shared host
  directories, and forwards `127.0.0.1:<sshPort>` to guest SSH port `22`.

## Prepare and launch offline

Build the VM while connected, with the workers and SSH public keys you need.
Keep the output link so Nix retains the launcher and its dependencies. The
launcher can then run directly without resolving flake inputs. For example,
from the framework checkout with the `downstreams/demo-lab` worktree present:

```sh
mkdir -p "$HOME/boreal-vm"
nix build ./downstreams/demo-lab#boreal-vm --override-input boreal . \
  --no-write-lock-file --out-link "$HOME/boreal-vm/result"
NIX_DISK_IMAGE="$HOME/boreal-vm/boreal-vm.qcow2" \
  "$HOME/boreal-vm/result/bin/run-boreal-vm"
```

Without `NIX_DISK_IMAGE`, the launcher uses `boreal.vm.diskImage`
(`./boreal-vm.qcow2` by default) in the current working directory. The disk
persists PostgreSQL Temporal state and guest state across restarts, and
temporary per-start store images are not removed automatically.

Use hardware acceleration (KVM on same-architecture Linux) when available.
The VM starts new executions in its own Temporal server. It does not resume
or synchronize executions from the remote lab. Provision the vault secrets
and any other inputs needed by the VM separately. Only workflows whose
dependencies are available locally can complete while disconnected.

The reference demo is Dispatch Desk: Alice hands a request to a shared worker
in a separate Temporal namespace, and each side uses its own vault. See the
downstream worktree's `README.md` for its SSH and fixture setup.

## Interact over SSH and shut down

From another terminal on the host:

```sh
# Interactive restricted lab shell:
ssh -p 2222 alice@127.0.0.1

# Upload a user secret from stdin without shell/history exposure:
ssh -p 2222 alice@127.0.0.1 "secrets set api_token" < ./api_token

# Inspect allowlisted Temporal/worker service health as restricted admin:
ssh -p 2222 admin@127.0.0.1 services

# Shut down the VM cleanly after work finishes:
ssh -p 2222 admin@127.0.0.1 poweroff
```
