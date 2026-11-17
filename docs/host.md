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
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  boreal = {
    enable = true;

    users = {
      alice = {
        keys = [ ]; # Add Alice's public SSH key.
        permissions = { read = true; write = false; };
        workers = {
          greet = {
            package = packages.lab-worker.greet;
            schedule = {
              enable = true;
              package = packages.lab-worker.greet-schedule;
            };
            permissions.write = [ "bob" ];
          };
          count = {
            package = packages.lab-worker.count;
            permissions.read = [ "bob" ];
          };
        };
      };
      bob = {
        keys = [ ]; # Add Bob's public SSH key.
        editMode = "vi"; # Optional: "emacs" (default), "vi", or "helix" (~/.borealrc).
        workers = {
          greet.package = packages.lab-worker.greet;
          count.package = packages.lab-worker.count;
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
      boreal.updater.enable = true;
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

Build the worker crate with `boreal.lib.buildRustWorker` and list every bin as
an output, plus `schema`:

```nix
outputs = [ "out" "greet" "greet-schedule" "count" "schema" ];
```

`out` is only the Cargo install prefix. Each other name except `schema` is a
bin moved to `$<name>/bin/<name>`. The `schema` bin is a build-time
intermediate: `postInstall` runs it and installs `schema.yaml` as the `schema`
output. The module writes those directories to `/etc/boreal/schemas.yaml`.

To register Temporal schedules, set `schedule.enable = true` and
`schedule.package` to the schedule output. The module starts
`boreal-schedule-<user>-<worker>.service`, a oneshot in that worker's network
namespace. The binary should call `boreal_worker::connect()`, upsert schedules
with the stock Temporal client, and exit. Schedules are rows in Temporal, not a
timer: the server starts the workflows, and the worker unit executes them.

## User secrets and restricted administration

Lab users connect over SSH into `boreal-shell` and manage their own secrets
without exposing secret contents on command lines or in Temporal history:

```sh
ssh alice@lab-metal-01 "secrets set api_token" < ./api_token
ssh alice@lab-metal-01
```

By contract, the downstream lab repository is cloned at `/etc/nixos` (owned by
`root`, tracking an `https://` upstream branch, with committed `flake.lock`
free of local `file://` or `path` overrides) and defines
`nixosConfigurations.<networking.hostName>`. Administrators operate allowlisted
services and two-step updates (`updater update` to hard-reset `/etc/nixos` to
upstream and prebuild the generation, then `updater schedule` to switch in the
background once no pipeline is running) through the restricted `admin` SSH
interface:

```sh
ssh admin@lab-metal-01 services
ssh admin@lab-metal-01 "updater update"
ssh admin@lab-metal-01 "updater schedule"
ssh admin@lab-metal-01 "updater status"
```

See [the updater documentation](../updater/README.md) for drain and recovery
semantics.
