{
  git,
  lib,
  rustPlatform,
}:

let
  fs = lib.fileset;
  src = fs.toSource {
    root = ../.;
    fileset =
      fs.difference
        (fs.unions [
          ../common
          ../updater
        ])
        (
          fs.unions [
            (fs.maybeMissing ../common/target)
            (fs.maybeMissing ../updater/target)
            (fs.maybeMissing ../updater/README.md)
          ]
        );
  };
in
rustPlatform.buildRustPackage {
  pname = "boreal-updater";
  version = "0.1.0";
  inherit src;
  sourceRoot = "${src.name}/updater";
  cargoLock.lockFile = ../updater/Cargo.lock;
  nativeCheckInputs = [ git ];
  meta.mainProgram = "boreal-updater";
}
