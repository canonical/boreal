{ self, lib }:
pkgs:
{
  workers,
  vault ? null,
}:
let
  system = pkgs.stdenv.hostPlatform.system;
  inherit (self.packages.${system}) boreal-broker boreal-shell;
  names = builtins.attrNames workers;
  validName = name: builtins.match "[a-z][a-z0-9_-]*" name != null;
  validBin =
    pkg:
    validName pkg.outputName
    && !(lib.elem pkg.outputName [
      "out"
      "schema"
    ]);
  yaml = pkgs.formats.yaml { };
  schemaDirs = lib.unique (map (worker: toString worker.schema) (builtins.attrValues workers));
  schemas = yaml.generate "boreal-local-schemas.yaml" { directories = schemaDirs; };
  permissions = yaml.generate "boreal-local-perms.yaml" {
    user = "local";
    permissions = {
      read = false;
      write = false;
    };
    workers = lib.mapAttrs (_: worker: {
      package = worker.outputName;
      permissions = {
        read = false;
        write = false;
      };
    }) workers;
  };
  catalog = pkgs.runCommand "boreal-local-catalog" { } ''
    install -Dm444 ${schemas} "$out/schemas.yaml"
    install -Dm444 ${permissions} "$out/users/local/perms.yaml"
  '';
  binOf = worker: "${worker}/bin/${worker.outputName}";
  # This is scoped to loading the exact archive from the Nix store, not pulls.
  archivePolicy = pkgs.writeText "boreal-local-image-policy.json" (
    builtins.toJSON {
      default = [ { type = "reject"; } ];
      transports.docker-archive."" = [ { type = "insecureAcceptAnything"; } ];
    }
  );
  image = pkgs.dockerTools.buildLayeredImage {
    name = "localhost/boreal-local";
    contents = [
      catalog
      pkgs.temporal-cli
      boreal-broker
    ]
    ++ builtins.attrValues workers
    ++ lib.optional (vault != null) vault;
    extraCommands = ''
      mkdir -p etc home/local/.config/boreal/secrets data ipc
      printf 'root:x:0:0:root:/root:/sbin/nologin\n' > etc/passwd
      printf 'root:x:0:\n' > etc/group
    '';
    config = {
      Env = [ "HOME=/home/local" ];
      WorkingDir = "/home/local";
    };
  };
in
assert lib.assertMsg (names != [ ]) "mkLocal: configure at least one worker";
assert lib.assertMsg (builtins.all (
  name: validName name && name != "vault" && !lib.hasSuffix "-vault" name
) names) "mkLocal: invalid worker name or reserved vault queue";
assert lib.assertMsg (builtins.all
  (worker: worker ? schema && validBin worker && !(worker.borealVault or false))
  (builtins.attrValues workers)
) "mkLocal: workers must be executable buildRustWorker outputs with schemas";
assert lib.assertMsg (
  vault == null || ((vault.borealVault or false) && validBin vault)
) "mkLocal: vault must be an executable buildRustVault output";
pkgs.writeShellApplication {
  name = "boreal-local";
  runtimeInputs = [
    pkgs.coreutils
    pkgs.util-linux
  ];
  text = ''
    podman_bin=${lib.getExe pkgs.podman}
    shell_bin=${lib.getExe boreal-shell}
    broker_bin=${lib.getExe boreal-broker}
    temporal_bin=${lib.getExe pkgs.temporal-cli}
    store_catalog=${catalog}
    archive=${image}
    archive_policy=${archivePolicy}
    image_ref=${image.imageName}:${image.imageTag}
    vault_bin=${if vault == null then "" else lib.escapeShellArg (binOf vault)}
    worker_names=(${lib.escapeShellArgs names})
    worker_bins=(${lib.escapeShellArgs (map (name: binOf workers.${name}) names)})
  ''
  + builtins.readFile ./local.sh;
}
