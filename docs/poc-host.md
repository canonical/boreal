# Disposable host example

This example deliberately has **no Temporal authentication or authorization**.
Use it only in an isolated test environment; any local user who can reach the
development server can access any namespace. It is not a shared-lab security
configuration.

A NixOS configuration can import this flake as `inputs.boreal` and add:

```nix
{ inputs, pkgs, ... }:
{
  imports = [ inputs.boreal.nixosModules.default ];

  boreal.poc = {
    enable = true;
    acknowledgeInsecureTemporal = true;
    users.alice.sshKeys = [ ]; # Add Alice's public key when needed.

    workers = {
      alice-greet = {
        user = "alice";
        package = inputs.boreal.packages.${pkgs.stdenv.hostPlatform.system}.lab-worker;
        environment = {
          TEMPORAL_ADDRESS = "localhost:7233";
          TEMPORAL_NAMESPACE = "alice";
          LAB_TASK_QUEUE = "alice-greet";
        };
      };
      alice-count = {
        user = "alice";
        package = inputs.boreal.packages.${pkgs.stdenv.hostPlatform.system}.lab-worker-alt;
        environment = {
          TEMPORAL_ADDRESS = "localhost:7233";
          TEMPORAL_NAMESPACE = "alice";
          LAB_TASK_QUEUE = "alice-count";
        };
      };
    };

    updater.enable = true;
  };
}
```

The host must also have its own hardware and network configuration. This module
does not start a Temporal server. For an isolated, temporary test, start the
development server with:

```sh
temporal server start-dev --headless --namespace alice
```

The workers reconnect if the server is not ready. In Alice's shell, use the
matching namespace and queue when starting the example workflow:

```sh
export TEMPORAL_NAMESPACE=alice
export LAB_TASK_QUEUE=alice-greet
nix run .#lab-client -- start greet demo-1 Example 5
nix run .#lab-client -- status demo-1
```

Run these commands from a checkout of this repository, or install `lab-client`
separately; the NixOS module does not add it to user profiles. The example input
is recorded in Temporal and must not contain secrets. `alice-count` requires the
matching `alice-count` queue. Do not mix the two packages on the same queue.

The updater's scheduling command accepts a public HTTPS Git repository, a full
commit, and a NixOS configuration name:

```text
sudo boreal-schedule-update <repository-url> <full-commit> <configuration-name>
sudo boreal-updater status
```

The revision must already be accessible in that repository. The service will
prebuild and then report `blocked`; it cannot switch or garbage-collect the
system. See [the updater documentation](../updater/README.md).
