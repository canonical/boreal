{ lib, rustPlatform }:

let
  fs = lib.fileset;
  src = fs.toSource {
    root = ../.;
    fileset =
      fs.difference
        (fs.unions [
          ../common
          ../shell
          ../templates/default/temporal/Cargo.toml
        ])
        (
          fs.unions [
            (fs.maybeMissing ../common/target)
            (fs.maybeMissing ../shell/target)
          ]
        );
  };
in
rustPlatform.buildRustPackage {
  pname = "boreal-shell";
  version = "0.1.0";
  inherit src;
  sourceRoot = "${src.name}/shell";
  cargoLock.lockFile = ../shell/Cargo.lock;
  meta.mainProgram = "boreal-shell";
}
