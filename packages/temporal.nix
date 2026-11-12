{
  lib,
  rustPlatform,
  pkg-config,
  protobuf,
  crate,
}:

rustPlatform.buildRustPackage {
  pname = crate;
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ../temporal;
    filter = path: type:
      baseNameOf path != "target"
      && baseNameOf path != "README.md"
      && lib.cleanSourceFilter path type;
  };
  cargoLock.lockFile = ../temporal/Cargo.lock;
  cargoBuildFlags = [ "--package" crate ];
  cargoTestFlags = [ "--package" crate ];
  nativeBuildInputs = [ pkg-config protobuf ];

  meta.mainProgram = crate;
}
