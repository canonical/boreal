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

  mkFleetCheck =
    { pkgs, hosts }:
    let
      failures = import ./fleet.nix { inherit lib; } hosts;
    in
    if failures != [ ] then
      throw "Boreal fleet check failed:\n${lib.concatMapStringsSep "\n" (line: "- ${line}") failures}"
    else
      pkgs.runCommand "boreal-fleet-check" { } "touch \"$out\"";

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
      versionEtc = evaluated.environment.etc."boreal/version";
      schemasEtc = evaluated.environment.etc."boreal/schemas.yaml";
      userPolicySources = map (user: evaluated.environment.etc."boreal/users/${user}/perms.yaml".source) (
        builtins.attrNames evaluated.boreal.users
      );
      schemaOutputs = lib.unique (
        lib.concatLists (
          lib.mapAttrsToList (
            _: userCfg: lib.mapAttrsToList (_: workerCfg: workerCfg.package.schema) userCfg.workers
          ) evaluated.boreal.users
        )
      );
    in
    if failed != [ ] then
      throw "Boreal lab check failed assertions:\n${failureText}"
    else
      assert evaluated.boreal.enable;
      assert versionEtc.user == "root" && versionEtc.group == "boreal" && versionEtc.mode == "0440";
      assert schemasEtc.user == "root" && schemasEtc.group == "boreal" && schemasEtc.mode == "0440";
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
          if grep -Eq 'ssh-' ${schemasEtc.source}; then
            echo "Unexpected key in schemas.yaml" >&2
            exit 1
          fi
          remarshal --if yaml --of json ${schemasEtc.source} > /dev/null
          for schema in ${lib.escapeShellArgs schemaOutputs}; do
            test -f "$schema/schema.yaml"
            remarshal --if yaml --of json "$schema/schema.yaml" > /dev/null
          done
          for policy in ${lib.escapeShellArgs userPolicySources}; do
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
      version ? null,
      ...
    }@args:
    let
      cargoManifest =
        let
          manifestPath = src + "/Cargo.toml";
        in
        if builtins.pathExists manifestPath then
          builtins.fromTOML (builtins.readFile manifestPath)
        else
          { };
      resolvedVersion =
        if version != null then
          version
        else
          cargoManifest.workspace.package.version or cargoManifest.package.version or "0.1.0";
      passthrough = builtins.removeAttrs args [
        "pname"
        "version"
        "src"
        "outputs"
        "cargoBuildFlags"
        "cargoTestFlags"
        "nativeBuildInputs"
        "postUnpack"
        "postInstall"
        "meta"
      ];
      outputs =
        args.outputs or [
          "out"
          "schema"
        ];
      binOutputs = builtins.filter (name: name != "out" && name != "schema") outputs;
    in
    assert lib.assertMsg (builtins.elem "out" outputs) "buildRustWorker: outputs must include out";
    assert lib.assertMsg (builtins.elem "schema" outputs)
      "buildRustWorker: outputs must include schema";
    pkgs.rustPlatform.buildRustPackage (
      passthrough
      // {
        inherit pname cargoLock;
        version = resolvedVersion;
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
        outputs = outputs;
        # stdenv fixup expands ${!output}. Hyphenated names such as
        # greet-schedule are valid Nix outputs but not bash identifiers.
        dontFixup = true;
        postInstall =
          (args.postInstall or "")
          + ''
            if [[ ! -x $out/bin/schema ]]; then
              echo "buildRustWorker: \$out/bin/schema was not installed" >&2
              exit 1
            fi
            schema_out="$(printenv schema)"
            mkdir -p "$schema_out"
            "$out/bin/schema" > "$schema_out/schema.yaml"
            rm -f "$out/bin/schema"
          ''
          + lib.concatMapStrings (name: ''
            if [[ ! -x "$out/bin/"${lib.escapeShellArg name} ]]; then
              echo "buildRustWorker: \$out/bin/${name} was not installed" >&2
              exit 1
            fi
            dest="$(printenv ${lib.escapeShellArg name})"
            mkdir -p "$dest/bin"
            mv "$out/bin/"${lib.escapeShellArg name} "$dest/bin/"${lib.escapeShellArg name}
          '') binOutputs
          + ''
            if command -v strip >/dev/null; then
              for output in $outputs; do
                prefix="$(printenv "$output")"
                find "$prefix" -type f -executable -exec strip -S -p {} + || true
              done
            fi
          '';
        meta = {
          mainProgram = pname;
        }
        // (args.meta or { });
      }
    );
}
