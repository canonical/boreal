{
  description = "Reference Boreal downstream lab deployment";

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
      publicKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINuoseDzo0mUwBthHFnKfNPK1EdJTrpv7boeC1ybMsty";
      mkTemporalCrate =
        pkgs: pname:
        boreal.lib.buildRustWorker pkgs {
          inherit pname;
          src = ./temporal;
          cargoLock.lockFile = ./temporal/Cargo.lock;
        };
    in
    {
      devShells = forAllSystems (pkgs: {
        default = boreal.lib.mkDevShell pkgs { };
      });

      packages = forAllSystems (
        pkgs:
        {
          lab-worker = mkTemporalCrate pkgs "lab-worker";
          lab-worker-alt = mkTemporalCrate pkgs "lab-worker-alt";
          lab-client = mkTemporalCrate pkgs "lab-client";
        }
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          boreal-vm = self.nixosConfigurations.boreal-vm.config.system.build.vm;
        }
      );

      nixosConfigurations.boreal-vm = boreal.lib.mkVm {
        system = "x86_64-linux";
        specialArgs = { inherit self; };
        modules = [
          ./lab.nix
          (
            { pkgs, ... }:
            {
              nix.settings.substituters = [
                "https://mirrors.bfsu.edu.cn/nix-channels/store"
                "https://mirror.sjtu.edu.cn/nix-channels/store"
              ];
              environment.systemPackages = [
                self.packages.${pkgs.stdenv.hostPlatform.system}.lab-client
              ];
              boreal.updater.enable = true;
              boreal.vm.adminKeys = [ publicKey ];
            }
          )
        ];
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
