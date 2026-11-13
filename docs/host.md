# Host configuration example

This example generates Boreal user accounts, worker units, and policy files, but
**does not yet enforce Temporal authentication or runtime permissions**. Any
local user who can reach a development server can still access any namespace
until host and server isolation are configured. Use it only in an isolated test
environment.

A NixOS configuration can import this flake as `inputs.boreal` and add:

```nix
{ inputs, ... }:
{
  imports = [ inputs.boreal.nixosModules.default ];

  boreal = {
    enable = true;
    users = {
      alice = {
        keys = [ ]; # Add Alice's public SSH key when needed.
        permissions = {
          read = true;
          write = false;
        };
        workers = {
          greet = {
            package = "lab-worker";
            permissions.write = [ "bob" ];
          };
          count = {
            package = "lab-worker-alt";
            permissions.read = [ "bob" ];
          };
        };
      };
      bob = {
        keys = [ ]; # Add Bob's public SSH key when needed.
        workers = {
          greet.package = "lab-worker";
          count.package = "lab-worker-alt";
        };
      };
    };

    updater.enable = true;
  };
}
```

Each worker's `package` name is resolved against the known Boreal worker
packages (`lab-worker` and `lab-worker-alt`). For user `<user>` and worker
`<worker>`, the module configures `boreal-worker-<user>-<worker>.service` with
`TEMPORAL_ADDRESS` (`boreal.temporalAddress`, default `127.0.0.1:7233`),
`TEMPORAL_NAMESPACE=<user>`, and `LAB_TASK_QUEUE=<user>-<worker>`.

Nix writes each user's resolved policy to `/etc/boreal/users/<user>/perms.yaml`
as a `root:boreal` file with mode `0440` under `0750` directories, keeping the
authoritative copy outside writable home directories. Policy inputs pass through
the Nix store and must never contain secrets.

The host must also have its own hardware and network configuration. This module
does not start a Temporal server. For an isolated, temporary test, start the
development server with:

```sh
temporal server start-dev --headless --ip 127.0.0.1 --namespace alice --namespace bob
```

The workers reconnect if the server is not ready. In Alice's shell, use the
matching namespace and queue when starting a workflow:

```sh
export TEMPORAL_ADDRESS=127.0.0.1:7233
export TEMPORAL_NAMESPACE=alice
export LAB_TASK_QUEUE=alice-greet
nix run .#lab-client -- start greet run-1 Example 5
nix run .#lab-client -- status run-1
```

Run these commands from a checkout of this repository, or install `lab-client`
separately; the NixOS module does not add it to user profiles. Workflow inputs
are recorded in Temporal and must not contain secrets. `count` requires the
matching `alice-count` queue. Do not mix differently registered worker packages
on the same queue.

The updater's scheduling command accepts a public HTTPS Git repository, a full
commit, and a NixOS configuration name:

```text
sudo boreal-schedule-update <repository-url> <full-commit> <configuration-name>
sudo boreal-updater status
```

The revision must already be accessible in that repository. The service will
prebuild and then report `blocked`; it cannot switch or garbage-collect the
system. See [the updater documentation](../updater/README.md).
