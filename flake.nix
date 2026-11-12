{
  description = "Boreal lab infrastructure proof of concept";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
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

      packages = forAllSystems (pkgs:
        (import ./packages { inherit pkgs; })
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          boreal-vm = self.nixosConfigurations.boreal-vm.config.system.build.vm;
        });
      checks = forAllSystems (pkgs: {
        modules = import ./tests/modules.nix { inherit nixpkgs pkgs; };
      });
      nixosModules.default = import ./modules;
      nixosConfigurations.poc = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          ./modules
          ./hosts/poc.nix
        ];
      };
      nixosConfigurations.boreal-vm = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          ./modules
          ./hosts/boreal-vm.nix
        ];
      };
      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
