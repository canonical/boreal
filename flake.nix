{
  description = "Beijing Boreal lab (synthetic workflows only)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/ac588ef1c914865dad517b337ea0eb9694a4021c";
    boreal = {
      url = "github:canonical/boreal/7c8bec59685d6c743c8c717be0b214f0522ed215";
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
        default = boreal.lib.mkDevShell pkgs {
          extraPackages = [ self.packages.${pkgs.stdenv.hostPlatform.system}.boreal-local ];
        };
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
          boreal-local = boreal.lib.mkLocal pkgs {
            workers.beijing = self.packages.${system}.lab-worker.beijing;
          };
        }
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          vm = self.nixosConfigurations.vm.config.system.build.vm;
        }
      );

      nixosConfigurations = {
        vm = boreal.lib.mkVm {
          system = "x86_64-linux";
          specialArgs = { inherit self; };
          modules = [
            ./lab.nix
            {
              boreal.vm = {
                systemName = "beijing";
                hostName = "beijing-vm";
                diskImage = "./beijing-vm.qcow2";
                # Optional: authorize your local public SSH key for VM testing:
                # devAuthorizedKeys = [ "ssh-ed25519 AAAAC3Nza..." ];
              };
            }
          ];
        };

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
