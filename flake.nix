{
  description = "Boreal Dispatch Desk manual test lab";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    boreal = {
      url = "git+ssh://git@github.com/canonical/boreal.git?ref=main";
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
      systemOf = pkgs: pkgs.stdenv.hostPlatform.system;
      temporalSrc = {
        src = ./temporal;
        cargoLock.lockFile = ./temporal/Cargo.lock;
      };
      buildWorker =
        pkgs: pname:
        boreal.lib.buildRustWorker pkgs (
          temporalSrc
          // {
            inherit pname;
            outputs = [
              "out"
              pname
              "schema"
            ];
          }
        );
    in
    {
      devShells = forAllSystems (pkgs: {
        default = boreal.lib.mkDevShell pkgs {
          extraPackages = nixpkgs.lib.optional (
            boreal.lib ? mkLocal
          ) self.packages.${systemOf pkgs}.boreal-local;
        };
      });

      packages = forAllSystems (
        pkgs:
        let
          system = systemOf pkgs;
          demo = self.packages.${system};
        in
        {
          demo-desk = buildWorker pkgs "demo-desk";
          demo-dispatch = buildWorker pkgs "demo-dispatch";
          demo-vault = boreal.lib.buildRustVault pkgs (temporalSrc // { pname = "demo-vault"; });
        }
        // nixpkgs.lib.optionalAttrs (boreal.lib ? mkLocal) {
          boreal-local = boreal.lib.mkLocal pkgs {
            workers = {
              desk = demo.demo-desk.demo-desk;
              dispatch = demo.demo-dispatch.demo-dispatch;
            };
            vault = {
              package = demo.demo-vault.demo-vault;
              secretDir = "./fixtures/dispatch-desk";
            };
          };
        }
        // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
          boreal-vm = self.nixosConfigurations.boreal-vm.config.system.build.vm;
        }
      );

      nixosConfigurations.boreal-vm = boreal.lib.mkVm {
        specialArgs = { inherit self; };
        modules = [
          ./lab.nix
          {
            boreal.vm = {
              hostName = "dispatch-desk";
              diskImage = "./dispatch-desk.qcow2";
              devAuthorizedKeys = [
                # Paste a disposable developer SSH public key here.
              ];
            };
          }
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
