{
  description = "Beijing Boreal lab (synthetic workflows only)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    boreal = {
      url = "github:canonical/boreal";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    lanzaboote = {
      url = "github:nix-community/lanzaboote/v1.2.0";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      boreal,
      lanzaboote,
      nixpkgs,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
        default = boreal.lib.mkDevShell pkgs {};
      });

      packages = forAllSystems (
        pkgs:
        let
          system = pkgs.stdenv.hostPlatform.system;
        in
        {
          lab-worker = boreal.lib.buildRustWorker pkgs {
            pname = "lab-worker";
            src = ./temporal;
            cargoLock.lockFile = ./temporal/Cargo.lock;
            outputs = [
              "out"
              "beijing"
              "schema"
            ];
          };
        }
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          vm = self.nixosConfigurations.vm.config.system.build.vm;
        }
      );

      nixosConfigurations = {
        chopstick = boreal.lib.mkHost {
          system = "x86_64-linux";
          specialArgs = { inherit self; };
          modules = [
            lanzaboote.nixosModules.lanzaboote
            ./lab.nix
            ./hosts/chopstick.nix
          ];
        };

        vm = boreal.lib.mkVm {
          system = "x86_64-linux";
          specialArgs = { inherit self; };
          modules = [
            ./lab.nix
            {
              boreal.role = "central";
              boreal.vm = {
                systemName = "beijing";
                hostName = "beijing-vm";
                diskImage = "./beijing-vm.qcow2";
              };
            }
          ];
        };

      };

      checks = forAllSystems (pkgs: {
        lab =
          assert import ./tests/authorized-keys.nix { inherit (pkgs) lib; };
          boreal.lib.mkLabCheck {
            inherit pkgs;
            specialArgs = { inherit self; };
            modules = [ ./lab.nix ];
          };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
