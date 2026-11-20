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
      # Public test SSH key shared by disposable demo accounts.
      publicKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINuoseDzo0mUwBthHFnKfNPK1EdJTrpv7boeC1ybMsty";
      demo = {
        centralIp = "192.0.2.1";
        peripheralIp = "192.0.2.2";
        port = 7443;
        clientDirectory = "/run/boreal-demo-client";
      };
      specialArgs = { inherit self demo publicKey; };
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
          lab-vault = boreal.lib.buildRustVault pkgs {
            pname = "lab-vault";
            src = ./temporal;
            cargoLock.lockFile = ./temporal/Cargo.lock;
          };
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

      nixosConfigurations = {
        boreal-vm = boreal.lib.mkVm {
          system = "x86_64-linux";
          inherit specialArgs;
          modules = [
            ./lab.nix
            ./demo-central.nix
          ];
        };
        compute-01 = boreal.lib.mkHost {
          system = "x86_64-linux";
          inherit specialArgs;
          modules = [
            ./lab.nix
            ./demo-peripheral.nix
          ];
        };
      };

      checks = forAllSystems (pkgs: {
        lab = boreal.lib.mkLabCheck {
          inherit pkgs specialArgs;
          modules = [
            ./lab.nix
            { boreal.remote.central = self.nixosConfigurations.boreal-vm.config.boreal.remote.central; }
          ];
        };
        fleet = boreal.lib.mkFleetCheck {
          inherit pkgs;
          hosts = { inherit (self.nixosConfigurations) boreal-vm compute-01; };
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
