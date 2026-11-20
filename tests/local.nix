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
  mkLocal =
    secretDir:
    self.lib.mkLocal pkgs {
      workers = { inherit (worker) greet count; };
      vault = {
        package = vault.lab-vault;
        inherit secretDir;
      };
    };
  rejects = path: !(builtins.tryEval (mkLocal path).drvPath).success;
in
assert builtins.all rejects [
  ""
  "/fixtures/alice"
  "../fixtures/alice"
  "fixtures/../alice"
  ./vm-worker
  "${pkgs.hello}/fixtures"
];
mkLocal "./fixtures/alice"
