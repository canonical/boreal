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
| Run workflows locally | Nix and a working rootless Podman setup on Linux. |
| Test a deployment in QEMU | Nix on the development machine; the guest runs NixOS. The starter VM targets `x86_64-linux`. |
| Host a deployed lab | NixOS on the servers running Boreal services. |

## Quickstart: run a workflow locally

With Git, Nix, and rootless Podman set up, create a lab from the starter
template and submit its example workflow:

```sh
nix flake new my-lab -t github:canonical/boreal
cd my-lab
git init
git add .

nix develop -c boreal-local run example --id local-1 --input-json '{"message":"hello"}'
```

`run` returns after submission. Once the workflow completes, check its result
and stop the local runtime:

```sh
nix develop -c boreal-local result local-1
nix develop -c boreal-local stop
```

The result is `Processed: hello`. To use the interactive shell, run
`nix develop -c boreal-local` without arguments. Local mode is for single-user
workflow development; use the [QEMU VM](docs/vm.md) to verify SSH access and
multi-user authorization before deploying.

## Where to go next

- **Use an existing lab:** [Run and inspect workflows over SSH](docs/workflows.md).
- **Build a lab:** Start with the [downstream template guide](templates/default/README.md),
  then see [local development](docs/local.md) and [worker packaging](worker/README.md).
- **Deploy a lab:** See [host configuration](docs/host.md),
  [remote workers](docs/remote-workers.md), and [safe updates](updater/README.md).
- **Look up an interface:** See the [flake reference](docs/flake-reference.md)
  or the [architecture and security boundaries](docs/architecture.md).
- **Change the framework:** See [contributing](CONTRIBUTING.md) and
  [coding agent guidance](AGENTS.md).
