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

      checks = forAllSystems (pkgs: {
        modules = import ./tests/modules.nix {
          inherit self nixpkgs pkgs;
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
