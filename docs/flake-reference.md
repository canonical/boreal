# Boreal flake reference

Downstreams consume the framework flake as `boreal` and pin it in their own
`flake.lock`. Boreal's framework flake tracks `nixos-26.05`; its `flake.lock`
selects the nixpkgs revision. Update pins deliberately rather than resolving
moving branches during deployment.

## Template and modules

| Output | Purpose |
| --- | --- |
| `templates.default` | Starter lab flake with an example worker and a QEMU VM. |
| `nixosModules.default` (alias `nixosModules.boreal`) | Lab service module, including `boreal.enable`, `boreal.users`, `boreal.admin`, and `boreal.updater`. |
| `nixosModules.vm` | Disposable QEMU guest settings under `boreal.vm`. |

## Library helpers

| Helper | Purpose |
| --- | --- |
| `mkHost { system, modules, specialArgs ? {} }` | Build a NixOS configuration with the default Boreal module. |
| `mkVm { system ? "x86_64-linux", modules, specialArgs ? {} }` | Build a QEMU guest configuration for offline execution and deployment checks. |
| `mkLabCheck { pkgs, modules, specialArgs ? {} }` | Validate NixOS assertions and generated policy and schema catalogs in `nix flake check`. |
| `mkFleetCheck { pkgs, hosts }` | Validate that evaluated hosts agree on worker placement and have exactly one central host. |
| `rustBin pkgs` | Access the pinned rust-overlay `rust-bin` toolchains for the supplied package set. |
| `mkDevShell pkgs { ... }` | Create the Rust development shell and link the pinned SDK at `.boreal/boreal-sdk`. |
| `buildRustWorker pkgs { ... }` | Build Temporal worker binaries and schema outputs using the pinned SDK. |
| `buildRustVault pkgs { ... }` | Build a separate, activity-only [vault worker package](vault.md#configure-a-vault). |

### Rust toolchains

`mkDevShell`, `buildRustWorker`, and `buildRustVault` default to the same
rust-overlay stable `latest` toolchain with `rust-analyzer`. Here `latest` is
selected by the pinned rust-overlay revision, not resolved anew on each build.
Review changes to the Boreal and rust-overlay inputs in the downstream
`flake.lock` when updating pins.

Boreal's own Rust packages and the default NixOS service packages also use
this overlay-backed toolchain. Explicit NixOS package options remain available
for downstream overrides.

Each helper accepts `rustToolchain` to override that default. The build helpers
also accept `rustPlatform`; supplying it takes precedence over `rustToolchain`
for the build. Set `rustPlatform = pkgs.rustPlatform` to opt out of the default
overlay-backed build platform and use the caller's nixpkgs platform instead.
Keep the shell toolchain aligned with the build platform when overriding it.
See the [starter guide](../templates/default/README.md#choosing-a-rust-toolchain)
for a shared toolchain-file example.

For examples, see [host configuration](host.md), [VM configuration](vm.md),
[workflow artifacts](artifacts.md), and the [Boreal SDK](../sdk/README.md).

## Packages

`packages.<system>` exposes `boreal-broker`, `boreal-shell`, `boreal-admin`,
`boreal-build`, and `boreal-updater` on `x86_64-linux` and `aarch64-linux`.
