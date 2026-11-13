{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "boreal-shell";
  version = "0.1.0";
  src = lib.cleanSource ../shell;
  cargoLock.lockFile = ../shell/Cargo.lock;
  meta.mainProgram = "boreal-shell";
}
