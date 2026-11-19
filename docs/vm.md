# Disposable QEMU test VM (`boreal.lib.mkVm`)

`boreal.nixosModules.vm` and `boreal.lib.mkVm` let any downstream lab build an
isolated QEMU test guest from the same `lab.nix` used for bare-metal hosts,
including network-namespace Temporal/PostgreSQL isolation, `boreal-broker`
policy and worker-gate enforcement, `boreal-shell`, and restricted
`boreal-admin`.

When `boreal.vm.devAuthorizedKeys` is set, `admin` and all `boreal.users`
accounts authorize those developer public keys for local testing; do not use
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

## Build and start (`downstreams/demo-lab` worktree example)

With the `downstreams/demo-lab` orphan branch checked out at
`downstreams/demo-lab`:

```sh
mkdir -p "$HOME/boreal-vm"
NIX_DISK_IMAGE="$HOME/boreal-vm/boreal-vm.qcow2" \
  nix run ./downstreams/demo-lab#boreal-vm --override-input boreal . --no-write-lock-file
```

Without `NIX_DISK_IMAGE`, the launcher uses `boreal.vm.diskImage`
(`./boreal-vm.qcow2` by default) in the current working directory. The disk
persists PostgreSQL Temporal state and guest state across restarts, and
temporary per-start store images are not removed automatically.

The reference demo also starts a `compute-01` peripheral container inside
the guest. See the downstream worktree's `README.md` for the fleet check,
how to launch with a fresh disk, and how to verify both `alice-greet` pollers.

## Interact over SSH and shut down

From another terminal on the host:

```sh
# Interactive restricted lab shell:
ssh -p 2222 alice@127.0.0.1

# Upload a user secret from stdin without shell/history exposure:
ssh -p 2222 alice@127.0.0.1 "secrets set api_token" < ./api_token

# Inspect allowlisted Temporal/worker service health as restricted admin:
ssh -p 2222 admin@127.0.0.1 services

# Shut down the disposable VM cleanly after work finishes:
ssh -p 2222 admin@127.0.0.1 poweroff
```
