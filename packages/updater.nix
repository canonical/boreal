{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "boreal-updater";
  version = "0.1.0";
  src = lib.cleanSourceWith {
    src = ../updater;
    filter = path: type:
      baseNameOf path != "target" && lib.cleanSourceFilter path type;
  };
  cargoLock.lockFile = ../updater/Cargo.lock;
  meta.mainProgram = "boreal-updater";
}
