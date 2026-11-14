# Downstream host and lab configuration

This example generates Boreal user accounts, worker units, `/etc/boreal/workers.yaml`,
and `/etc/boreal/users/<user>/perms.yaml`, but **does not yet enforce Temporal
authentication or runtime permissions**. Any local user who can reach a
development server can still access any namespace until host and server
isolation are configured. Use it only in an isolated test environment.

A downstream flake can take `boreal` as an input, define a shared `lab.nix`
for both its disposable test VM and bare-metal hosts, and wire host-specific
settings separately:

```nix
# lab.nix
{ self, pkgs, ... }:
let
  myPackages = self.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  boreal = {
    enable = true;

    workerPackages = {
      lab-worker = {
        package = myPackages.lab-worker;
        workflowType = "DemoWorkflow";
        description = "Run the greeting workflow";
        args = [
          { name = "name"; type = "string"; description = "Recipient name"; }
          {
            name = "wait_seconds";
            type = "int";
            min = 1;
            max = 7200;
            default = 20;
            description = "Timer delay in seconds";
          }
        ];
      };
      lab-worker-alt = {
        package = myPackages.lab-worker-alt;
        workflowType = "CountWorkflow";
        description = "Count Unicode scalar values";
        args = [
          { name = "name"; type = "string"; description = "Input text"; }
          {
            name = "wait_seconds";
            type = "int";
            min = 1;
            max = 7200;
            default = 20;
            description = "Timer delay in seconds";
          }
        ];
      };
    };

    users = {
      alice = {
        keys = [ ]; # Add Alice's public SSH key.
        permissions = { read = true; write = false; };
        workers = {
          greet = { package = "lab-worker"; permissions.write = [ "bob" ]; };
          count = { package = "lab-worker-alt"; permissions.read = [ "bob" ]; };
        };
      };
      bob = {
        keys = [ ]; # Add Bob's public SSH key.
        workers = {
          greet.package = "lab-worker";
          count.package = "lab-worker-alt";
        };
      };
    };
  };
}
```

In the downstream `flake.nix`, combine `lab.nix` with hardware/network modules
using `boreal.lib.mkHost` (optionally adding `boreal.nixosModules.chinaMirrors`)
and validate the policy in CI with `boreal.lib.mkLabCheck`:

```nix
nixosConfigurations.lab-metal-01 = boreal.lib.mkHost {
  system = "x86_64-linux";
  specialArgs = { inherit self; };
  modules = [
    ./lab.nix
    ./hosts/lab-metal-01.nix
    { boreal.updater.enable = true; }
  ];
};
```

For user `<user>` and worker `<worker>`, Boreal configures
`boreal-worker-<user>-<worker>.service` with `TEMPORAL_ADDRESS`
(`boreal.temporalAddress`, default `127.0.0.1:7233`),
`TEMPORAL_NAMESPACE=<user>`, and `LAB_TASK_QUEUE=<user>-<worker>`. Do not mix
differently registered worker packages on the same queue.

The updater's scheduling command accepts a public HTTPS Git repository, a full
commit, and a NixOS configuration name:

```text
sudo boreal-schedule-update <repository-url> <full-commit> <configuration-name>
sudo boreal-updater status
```

The service prebuilds the configuration and then reports `blocked`; it cannot
switch or garbage-collect the system. See [the updater documentation](../updater/README.md).
