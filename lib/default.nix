{ self, nixpkgs }:

let
  inherit (nixpkgs) lib;

  borealWorkerSrc = lib.cleanSourceWith {
    src = "${self}/worker";
    filter =
      path: type:
      baseNameOf path != "target"
      && baseNameOf path != "Cargo.lock"
      && baseNameOf path != "README.md"
      && lib.cleanSourceFilter path type;
  };
in
{
  mkHost =
    {
      system,
      modules,
      specialArgs ? { },
    }:
    lib.nixosSystem {
      inherit system specialArgs;
      modules = [ self.nixosModules.default ] ++ modules;
    };

  mkVm =
    {
      system ? "x86_64-linux",
      modules,
      specialArgs ? { },
    }:
    lib.nixosSystem {
      inherit system specialArgs;
      modules = [
        self.nixosModules.default
        self.nixosModules.vm
      ]
      ++ modules;
    };

  mkLabCheck =
    {
      pkgs,
      modules,
      specialArgs ? { },
    }:
    let
      system = pkgs.stdenv.hostPlatform.system;
      evaluated =
        (lib.nixosSystem {
          inherit system specialArgs;
          modules = [
            self.nixosModules.default
            {
              boot.isContainer = true;
              networking.hostName = "boreal-check";
              system.stateVersion = "26.05";
            }
          ]
          ++ modules;
        }).config;
      failed = builtins.filter (entry: !entry.assertion) evaluated.assertions;
      failureText = lib.concatMapStringsSep "\n" (entry: "- ${entry.message}") failed;
      workersEtc = evaluated.environment.etc."boreal/workers.yaml";
      userPolicySources = map (user: evaluated.environment.etc."boreal/users/${user}/perms.yaml".source) (
        builtins.attrNames evaluated.boreal.users
      );
      allYamlSources = [ workersEtc.source ] ++ userPolicySources;
    in
    if failed != [ ] then
      throw "Boreal lab check failed assertions:\n${failureText}"
    else
      assert evaluated.boreal.enable;
      assert workersEtc.user == "root" && workersEtc.group == "boreal" && workersEtc.mode == "0440";
      assert builtins.all (
        user:
        let
          etcEntry = evaluated.environment.etc."boreal/users/${user}/perms.yaml";
        in
        etcEntry.user == "root" && etcEntry.group == "boreal" && etcEntry.mode == "0440"
      ) (builtins.attrNames evaluated.boreal.users);
      pkgs.runCommand "boreal-lab-check"
        {
          nativeBuildInputs = [ pkgs.remarshal ];
        }
        ''
          set -eu
          for policy in ${lib.escapeShellArgs allYamlSources}; do
            if grep -Eq 'ssh-|/nix/store/' "$policy"; then
              echo "Unexpected key or store path in $policy" >&2
              exit 1
            fi
            remarshal --if yaml --of json "$policy" > /dev/null
          done
          touch "$out"
        '';

  mkDevShell =
    pkgs:
    {
      extraPackages ? [ ],
      extraShellHook ? "",
    }:
    pkgs.mkShell {
      packages =
        (with pkgs; [
          cargo
          clippy
          nixfmt
          pkg-config
          protobuf
          rust-analyzer
          rustc
          rustfmt
          temporal-cli
        ])
        ++ extraPackages;
      shellHook = ''
        mkdir -p .boreal
        ln -sfn ${borealWorkerSrc} .boreal/boreal-worker
        ${extraShellHook}
      '';
    };

  buildRustWorker =
    pkgs:
    {
      pname,
      src,
      cargoLock,
      version ? "0.1.0",
      ...
    }@args:
    let
      passthrough = builtins.removeAttrs args [
        "pname"
        "version"
        "src"
        "cargoBuildFlags"
        "cargoTestFlags"
        "nativeBuildInputs"
        "postUnpack"
        "meta"
      ];
    in
    pkgs.rustPlatform.buildRustPackage (
      passthrough
      // {
        inherit pname version cargoLock;
        src = lib.cleanSourceWith {
          inherit src;
          filter =
            path: type:
            baseNameOf path != "target"
            && baseNameOf path != ".boreal"
            && baseNameOf path != "README.md"
            && lib.cleanSourceFilter path type;
        };
        cargoBuildFlags =
          args.cargoBuildFlags or [
            "--package"
            pname
          ];
        cargoTestFlags =
          args.cargoTestFlags or [
            "--package"
            pname
          ];
        nativeBuildInputs = [
          pkgs.pkg-config
          pkgs.protobuf
        ]
        ++ (args.nativeBuildInputs or [ ]);
        postUnpack = ''
          mkdir -p "$NIX_BUILD_TOP/.boreal" "$sourceRoot/.boreal"
          ln -sfn ${borealWorkerSrc} "$NIX_BUILD_TOP/.boreal/boreal-worker"
          ln -sfn ${borealWorkerSrc} "$sourceRoot/.boreal/boreal-worker"
        ''
        + (args.postUnpack or "");
        meta = {
          mainProgram = pname;
        }
        // (args.meta or { });
      }
    );
}
