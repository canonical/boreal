{
  lib,
  openssl,
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
          ../templates/default/temporal/Cargo.toml
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
  nativeCheckInputs = [ openssl ];
  checkFlags = [ "--include-ignored" ];
  meta.mainProgram = "boreal-broker";
}
