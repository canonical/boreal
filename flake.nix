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
        pkgs: pname: bins:
        boreal.lib.buildRustWorker pkgs {
          inherit pname;
          src = ./temporal;
          cargoLock.lockFile = ./temporal/Cargo.lock;
          outputs = [
            "out"
            "schema"
          ]
          ++ bins;
        };
    in
    {
      devShells = forAllSystems (pkgs: {
        default = boreal.lib.mkDevShell pkgs { };
      });

      packages = forAllSystems (
        pkgs:
        {
          lab-worker = mkTemporalCrate pkgs "lab-worker" [
            "lab-worker"
            "scheduler"
          ];
          lab-worker-alt = mkTemporalCrate pkgs "lab-worker-alt" [ "lab-worker-alt" ];
          lab-client = pkgs.rustPlatform.buildRustPackage {
            pname = "lab-client";
            version = (builtins.fromTOML (builtins.readFile ./temporal/Cargo.toml)).workspace.package.version;
            src = ./temporal;
            cargoLock.lockFile = ./temporal/Cargo.lock;
            cargoBuildFlags = [
              "--package"
              "lab-client"
            ];
            cargoTestFlags = [
              "--package"
              "lab-client"
            ];
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.protobuf
            ];
            postUnpack = ''
              mkdir -p "$NIX_BUILD_TOP/.boreal" "$sourceRoot/.boreal"
              ln -sfn ${boreal}/worker "$NIX_BUILD_TOP/.boreal/boreal-worker"
              ln -sfn ${boreal}/worker "$sourceRoot/.boreal/boreal-worker"
            '';
          };
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
