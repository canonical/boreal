{
  description = "Boreal NixOS and Temporal lab framework";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      rust = import ./lib/rust.nix { inherit rust-overlay; };
      borealLib = import ./lib { inherit self nixpkgs rust; };
      borealModule =
        { pkgs, ... }:
        {
          imports = [ ./modules ];
          _module.args.borealRustPlatform = rust.platform pkgs (rust.toolchain pkgs);
        };
    in
    {
      lib = borealLib;

      nixosModules = {
        default = borealModule;
        boreal = borealModule;
        vm = import ./modules/vm.nix;
      };

      templates.default = {
        path = ./templates/default;
        description = "Starter Boreal downstream lab with Temporal Rust worker and QEMU VM";
      };

      packages = forAllSystems (
        pkgs:
        import ./packages {
          inherit pkgs;
          rustPlatform = rust.platform pkgs (rust.toolchain pkgs);
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages =
            with pkgs;
            [
              nixfmt
              pkg-config
              protobuf
              temporal-cli
            ]
            ++ [ (rust.toolchain pkgs) ];
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
          rust = import ./tests/rust.nix { inherit self pkgs; };
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
