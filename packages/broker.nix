{
  lib,
  pkg-config,
  protobuf,
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
          ../broker
        ])
        (
          fs.unions [
            (fs.maybeMissing ../common/target)
            (fs.maybeMissing ../broker/target)
          ]
        );
  };
in
rustPlatform.buildRustPackage {
  pname = "boreal-broker";
  version = "0.1.0";
  inherit src;
  sourceRoot = "${src.name}/broker";
  cargoLock.lockFile = ../broker/Cargo.lock;
  nativeBuildInputs = [
    pkg-config
    protobuf
  ];
  meta.mainProgram = "boreal-broker";
}
