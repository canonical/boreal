{ self, pkgs }:

let
  worker = self.lib.buildRustWorker pkgs {
    pname = "lab-worker";
    src = ./vm-worker;
    cargoLock.lockFile = ./vm-worker/Cargo.lock;
    outputs = [
      "out"
      "greet"
      "count"
      "schema"
    ];
  };
  vault = self.lib.buildRustVault pkgs {
    pname = "lab-vault";
    src = ./vm-worker;
    cargoLock.lockFile = ./vm-worker/Cargo.lock;
  };
in
self.lib.mkLocal pkgs {
  workers = { inherit (worker) greet count; };
  vault = vault.lab-vault;
}
