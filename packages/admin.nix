{ lib, rustPlatform }:

let
  fs = lib.fileset;
  src = fs.toSource {
    root = ../.;
    fileset =
      fs.difference
        (fs.unions [
          ../common
          ../admin
          ../shell
          ../templates/default/temporal/Cargo.toml
        ])
        (
          fs.unions [
            (fs.maybeMissing ../common/target)
            (fs.maybeMissing ../admin/target)
            (fs.maybeMissing ../shell/target)
          ]
        );
  };
in
rustPlatform.buildRustPackage {
  pname = "boreal-admin";
  version = "0.1.0";
  inherit src;
  sourceRoot = "${src.name}/admin";
  cargoLock.lockFile = ../admin/Cargo.lock;
  meta.mainProgram = "boreal-admin";
}
