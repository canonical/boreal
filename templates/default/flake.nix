{
  description = "Starter Boreal downstream lab";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    boreal = {
      url = "github:canonical/boreal";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      boreal,
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
        default = boreal.lib.mkDevShell pkgs { };
      });

      packages = forAllSystems (
        pkgs:
        {
          example-worker = boreal.lib.buildRustWorker pkgs {
            pname = "example-worker";
            src = ./temporal;
            cargoLock.lockFile = ./temporal/Cargo.lock;
            outputs = [
              "out"
              "example"
              "schema"
            ];
          };
        }
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          vm = self.nixosConfigurations.vm.config.system.build.vm;
        }
      );

      nixosConfigurations = {
        # Disposable local test VM (`nix run .#vm`):
        vm = boreal.lib.mkVm {
          system = "x86_64-linux";
          specialArgs = { inherit self; };
          modules = [
            ./lab.nix
            {
              boreal.updater.enable = true;
              boreal.vm = {
                systemName = "lab";
                hostName = "lab-vm";
                diskImage = "./lab-vm.qcow2";
                # Optional: authorize your local public SSH key for VM testing:
                # devAuthorizedKeys = [ "ssh-ed25519 AAAAC3Nza..." ];
              };
            }
          ];
        };

        # Add bare-metal or production NixOS hosts here when deploying:
        # lab-01 = boreal.lib.mkHost {
        #   system = "x86_64-linux";
        #   specialArgs = { inherit self; };
        #   modules = [
        #     ./lab.nix
        #     ./hosts/lab-01.nix
        #     { boreal.updater.enable = true; }
        #   ];
        # };
      };

      checks = forAllSystems (pkgs: {
        lab = boreal.lib.mkLabCheck {
          inherit pkgs;
          specialArgs = { inherit self; };
          modules = [ ./lab.nix ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
