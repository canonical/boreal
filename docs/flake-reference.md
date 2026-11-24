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
| `mkDevShell pkgs { ... }` | Create the Rust development shell and link the pinned SDK at `.boreal/boreal-sdk`. |
| `buildRustWorker pkgs { ... }` | Build Temporal worker binaries and schema outputs using the pinned SDK. |
| `buildRustVault pkgs { ... }` | Build a separate, activity-only [vault worker package](vault.md#configure-a-vault). |

For examples, see [host configuration](host.md), [VM configuration](vm.md),
[workflow artifacts](artifacts.md), and the [Boreal SDK](../sdk/README.md).

## Packages

`packages.<system>` exposes `boreal-broker`, `boreal-shell`, `boreal-admin`,
`boreal-build`, and `boreal-updater` on `x86_64-linux` and `aarch64-linux`.
