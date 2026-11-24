{
  description = "Boreal NixOS and Temporal lab framework";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      borealLib = import ./lib { inherit self nixpkgs; };
    in
    {
      lib = borealLib;

      nixosModules = {
        default = import ./modules;
        boreal = import ./modules;
        vm = import ./modules/vm.nix;
      };

      templates.default = {
        path = ./templates/default;
        description = "Starter Boreal downstream lab with Temporal Rust worker and QEMU VM";
      };

      packages = forAllSystems (pkgs: import ./packages { inherit pkgs; });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            clippy
            nixfmt
            pkg-config
            protobuf
            rust-analyzer
            rustc
            rustfmt
            temporal-cli
          ];
        };
      });

      checks = forAllSystems (
        pkgs:
        let
          system = pkgs.stdenv.hostPlatform.system;
          templateOutputs = nixpkgs.lib.fix (
            templateSelf:
            (import ./templates/default/flake.nix).outputs {
              self = templateSelf;
              boreal = self;
              inherit nixpkgs;
            }
          );
        in
        {
          modules = import ./tests/modules.nix {
            inherit self nixpkgs pkgs;
          };
          template = builtins.seq templateOutputs.devShells.${system}.default.drvPath (
            templateOutputs.checks.${system}.lab
          );
          artifacts = import ./tests/artifacts.nix { inherit pkgs; };
        }
        // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
          vm = import ./tests/vm.nix {
            inherit self pkgs;
          };
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
