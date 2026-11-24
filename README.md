# Boreal

Boreal is a thin wrapper around NixOS and Temporal for running shared workflow
labs over SSH. It uses NixOS to make deployments quick and repeatable. You write
workflows and activities the usual Temporal way; Boreal handles deployment,
worker configuration, and lab access.

This repository is the framework. Each lab is a separate **downstream** Nix
flake that pins Boreal and defines its own users, workers, and permissions.

## Requirements

**NixOS is required on deployment hosts, not on your development machine.**

| To… | You need… |
| --- | --- |
| Connect to an existing lab | An SSH client and a lab account. No Nix required. |
| Develop Boreal or a downstream lab | The Nix package manager with flakes enabled on `x86_64-linux` or `aarch64-linux`. |
| Run workflows offline or test a deployment | Nix on an `x86_64-linux` machine to build the starter QEMU VM. KVM is recommended. |
| Host a deployed lab | NixOS on the servers running Boreal services. |

## Quickstart: run a workflow locally

With Git and Nix, create a lab from the starter template:

```sh
nix flake new my-lab -t github:canonical/boreal
cd my-lab
git init
git add .
```

Add your SSH public key to `boreal.vm.devAuthorizedKeys` in `flake.nix`. While
connected, build the VM and keep the output link. Then launch it directly,
even when the lab server is unreachable:

```sh
nix build .#vm --out-link result-vm
NIX_DISK_IMAGE="$PWD/lab-vm.qcow2" ./result-vm/bin/run-lab-vm
```

In another terminal, submit a workflow over SSH and check its result:

```sh
ssh -T -p 2222 alice@127.0.0.1 'run example --id local-1 --input-json -' <<'JSON'
{"message":"hello"}
JSON
ssh -T -p 2222 alice@127.0.0.1 'result local-1'
```

`run` returns after submission. Once the workflow completes, `result` returns
`Processed: hello`. See the [VM guide](docs/vm.md) for shutdown, persistence, and
the limits of offline execution.

## Where to go next

- **Use an existing lab:** [Run and inspect workflows over SSH](docs/workflows.md).
- **Build a lab:** Start with the [downstream template guide](templates/default/README.md),
  then see the [VM guide](docs/vm.md) and the [Boreal SDK](sdk/README.md).
- **Deploy a lab:** See [host configuration](docs/host.md),
  [remote workers](docs/remote-workers.md), [artifact storage](docs/artifacts.md),
  and [safe updates](updater/README.md).
- **Look up an interface:** See the [flake reference](docs/flake-reference.md)
  or the [architecture and security boundaries](docs/architecture.md).
- **Change the framework:** See [contributing](CONTRIBUTING.md) and
  [coding agent guidance](AGENTS.md).
