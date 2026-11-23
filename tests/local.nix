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
  users = {
    alice = {
      workers.desk.package = worker.greet;
      vault.package = vault.lab-vault;
    };
    shared = {
      workers.desk = {
        package = worker.count;
        permissions.write = [ "alice" ];
      };
      vault.package = vault.lab-vault;
    };
  };
  mkLocalOwners = caller: users: self.lib.mkLocalOwners pkgs { inherit caller users; };
in
assert builtins.all rejects [
  ""
  "/fixtures/alice"
  "../fixtures/alice"
  "fixtures/../alice"
  ./vm-worker
  "${pkgs.hello}/fixtures"
];
assert (builtins.tryEval (mkLocalOwners "alice" users).drvPath).success;
assert !(builtins.tryEval (mkLocalOwners "missing" users).drvPath).success;
assert !(builtins.tryEval (mkLocalOwners "alice" (users // { root = { }; })).drvPath).success;
assert
  !(builtins.tryEval
    (mkLocalOwners "alice" (
      pkgs.lib.recursiveUpdate users {
        alice.vault.secretDir = "./fixtures";
      }
    )).drvPath
  ).success;
mkLocal "./fixtures/alice"
