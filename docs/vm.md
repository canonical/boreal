# Disposable QEMU test VM (`boreal.lib.mkVm`)

`boreal.nixosModules.vm` and `boreal.lib.mkVm` let any downstream lab build an
isolated QEMU test guest from the same `lab.nix` used for bare-metal hosts.

This QEMU VM is a development environment, not a secure shared lab. It runs an
unauthenticated Temporal development server (`temporal-dev`) without runtime
policy enforcement; local guest accounts can still reach each other's Temporal
namespaces directly even though Nix generates `/etc/boreal/workers.yaml` and
`/etc/boreal/users/<user>/perms.yaml`. When `boreal.vm.devAuthorizedKeys` is
set, `admin` and all `boreal.users` accounts authorize those developer public
keys for convenient local testing. Use only dummy data and trusted test users.

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
- Registers `--namespace <user>` on `temporal-dev.service` for every user in
  `boreal.users` (defaulting to `--namespace default` when empty).
- Orders all `boreal-worker-<user>-<worker>` units after `temporal-dev.service`
  with bounded restart limits.
- Disables `system.switch.enable` so the VM configuration can never switch a
  host machine, uses an isolated Nix store image, disables shared host
  directories, and forwards `127.0.0.1:<sshPort>` to guest SSH port `22`.

## Build and start (`downstreams/demo-lab` worktree example)

With the `downstreams/demo-lab` orphan branch checked out at
`downstreams/demo-lab`:

```sh
mkdir -p "$HOME/boreal-vm"
NIX_DISK_IMAGE="$HOME/boreal-vm/boreal-vm.qcow2" \
  nix run ./downstreams/demo-lab#boreal-vm
```

Without `NIX_DISK_IMAGE`, the launcher uses `boreal.vm.diskImage`
(`./boreal-vm.qcow2` by default) in the current working directory. The disk
persists the Temporal SQLite database and guest state across restarts, and
temporary per-start store images are not removed automatically.

## Inspect generated catalogs and shut down

From another terminal on the host:

```sh
ssh -i ~/.ssh/self -p 2222 alice@127.0.0.1
```

Inside the guest:

```sh
cat /etc/boreal/workers.yaml
cat /etc/boreal/users/alice/perms.yaml
systemctl status temporal-dev
```

To shut down the VM cleanly after test work has finished:

```sh
ssh -i ~/.ssh/self -p 2222 admin@127.0.0.1 'sudo systemctl poweroff'
```
