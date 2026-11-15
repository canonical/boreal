# Downstream host and lab configuration

A downstream flake (bootstrapped with `nix flake new my-lab -t github:canonical/boreal`
or maintained on a `downstreams/<name>` orphan branch) takes `boreal` as an
input, defines a shared `lab.nix` for both its disposable test VM and
bare-metal hosts, and wires host-specific settings separately. Both
`boreal.lib.buildRustWorker` and `boreal.lib.mkDevShell` automatically link
`${boreal}/worker` at `.boreal/boreal-worker` from the revision locked in
`flake.lock`, so downstream Rust crates only pin `boreal` in `flake.nix` /
`flake.lock`:

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
        editMode = "vi"; # Optional: "emacs" (default), "vi", or "helix" (~/.borealrc).
        workers = {
          greet.package = "lab-worker";
          count.package = "lab-worker-alt";
        };
      };
    };
  };
}
```

In the downstream `flake.nix`, combine `lab.nix` with hardware/network modules,
restricted `boreal.admin`, and bounded `boreal.updater` using
`boreal.lib.mkHost`, and validate the policy in CI with
`boreal.lib.mkLabCheck`:

```nix
# flake.nix (excerpt)
inputs = {
  nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  boreal = {
    url = "github:canonical/boreal";
    inputs.nixpkgs.follows = "nixpkgs";
  };
};

# Inside outputs:
nixosConfigurations.lab-metal-01 = boreal.lib.mkHost {
  system = "x86_64-linux";
  specialArgs = { inherit self; };
  modules = [
    ./lab.nix
    ./hosts/lab-metal-01.nix
    {
      boreal.admin = {
        enable = true;
        keys = [ "ssh-ed25519 AAAAC3Nza..." ];
      };
      boreal.updater = {
        enable = true;
        repository = "https://github.com/example/my-lab.git";
        branch = "main";
        configurationName = "lab-metal-01";
      };
    }
  ];
};
```

For user `<user>` and worker `<worker>`, Boreal isolates PostgreSQL and
`services.temporal` inside `/run/netns/boreal-internal` and configures
`boreal-worker-<user>-<worker>.service` inside
`/run/netns/boreal-worker-<user>-<worker>` with `TEMPORAL_ADDRESS`
(`boreal.temporalAddress`, default `127.0.0.1:7233`),
`TEMPORAL_NAMESPACE=<user>`, and `LAB_TASK_QUEUE=<user>-<worker>`. Do not mix
differently registered worker packages on the same queue.

## User secrets and restricted administration

Lab users connect over SSH into `boreal-shell` and manage their own secrets
without exposing secret contents on command lines or in Temporal history:

```sh
ssh alice@lab-metal-01 "secrets set api_token" < ./api_token
ssh alice@lab-metal-01
```

Because `main` only provides the Boreal framework (no `nixosConfigurations`),
`boreal.updater` always targets a **40-digit commit on the downstream lab
repository** (and reviewed `branch` when set) whose committed `flake.lock` pins
`boreal` without local `file://` or `path` overrides. Administrators operate
allowlisted services and gated updates through the restricted `admin` SSH
interface:

```sh
ssh admin@lab-metal-01 services
ssh admin@lab-metal-01 "update schedule <40-digit-downstream-commit>"
ssh admin@lab-metal-01 "update prebuild"
ssh admin@lab-metal-01 "update apply"
ssh admin@lab-metal-01 "update status"
```

See [the updater documentation](../updater/README.md) for drain and recovery
semantics.
