{ self, pkgs }:

let
  rustBin = self.lib.rustBin pkgs;
  defaultToolchain = rustBin.stable.latest.default.override { extensions = [ "rust-analyzer" ]; };
  customToolchain = rustBin.stable.latest.minimal;
  fixture = {
    pname = "rust-test-worker";
    src = ./vm-worker;
    cargoLock.lockFile = ./vm-worker/Cargo.lock;
  };
  shell = args: self.lib.mkDevShell pkgs args;
  worker = args: self.lib.buildRustWorker pkgs (fixture // args);
  vault = args: self.lib.buildRustVault pkgs (fixture // args);
  countInput =
    input: drv:
    builtins.length (
      builtins.filter (candidate: toString candidate == toString input) drv.nativeBuildInputs
    );
  # makeRustPlatform receives the same toolchain for both rustc and cargo.
  usesToolchain = toolchain: drv: countInput toolchain drv == 2;
  usesNixpkgsPlatform =
    drv:
    countInput pkgs.cargo drv == 1
    && countInput pkgs.rustc drv == 1
    && countInput customToolchain drv == 0;
  custom = {
    rustToolchain = customToolchain;
  };
  platformOverride = custom // {
    rustPlatform = pkgs.rustPlatform;
  };
  system = pkgs.stdenv.hostPlatform.system;
  frameworkPackages = with self.packages.${system}; [
    boreal-admin
    boreal-broker
    boreal-shell
    boreal-updater
  ];
  host =
    overrides:
    self.lib.mkHost {
      inherit system;
      modules = [
        {
          boot.isContainer = true;
          networking.hostName = "boreal-rust-check";
          system.stateVersion = "26.05";
        }
        overrides
      ];
    };
in
assert toString defaultToolchain != toString customToolchain;
assert countInput defaultToolchain self.devShells.${system}.default == 1;
assert builtins.all (usesToolchain defaultToolchain) frameworkPackages;
assert countInput defaultToolchain (shell { }) == 1;
assert usesToolchain defaultToolchain (worker { });
assert usesToolchain defaultToolchain (vault { });
assert countInput customToolchain (shell custom) == 1;
assert countInput defaultToolchain (shell custom) == 0;
assert usesToolchain customToolchain (worker custom);
assert usesToolchain customToolchain (vault custom);
assert usesNixpkgsPlatform (worker platformOverride);
assert usesNixpkgsPlatform (vault platformOverride);
assert
  (host { }).config.boreal.shellPackage.drvPath == self.packages.${system}.boreal-shell.drvPath;
assert
  (host { boreal.shellPackage = pkgs.hello; }).config.boreal.shellPackage.drvPath
  == pkgs.hello.drvPath;
pkgs.runCommand "boreal-rust-check" { } ''
  touch "$out"
''
