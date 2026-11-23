{ self, lib }:
pkgs:
{
  caller,
  users,
}:
let
  system = pkgs.stdenv.hostPlatform.system;
  inherit (self.packages.${system}) boreal-broker boreal-shell;
  inherit (import ./catalog.nix { inherit lib; }) userPolicy;
  owners = builtins.attrNames users;
  normalizeVault = vault: if lib.isDerivation vault then { package = vault; } else vault;
  vaults = lib.filterAttrs (_: vault: vault != null) (
    lib.mapAttrs (_: user: normalizeVault (user.vault or null)) users
  );
  vaultOwners = builtins.attrNames vaults;
  secretDir = if caller == null then vaults.local.secretDir or null else null;
  relativeDir = if secretDir == null then "" else lib.removePrefix "./" secretDir;
  safeComponent = part: part != "." && part != ".." && builtins.match "[A-Za-z0-9._-]+" part != null;
  workers = lib.concatLists (
    lib.mapAttrsToList (
      owner: user:
      lib.mapAttrsToList (name: worker: {
        inherit owner name;
        inherit (worker) package;
      }) (user.workers or { })
    ) users
  );
  validName = name: builtins.match "[a-z][a-z0-9_-]*" name != null;
  validBin =
    pkg:
    validName pkg.outputName
    && !(lib.elem pkg.outputName [
      "out"
      "schema"
    ]);
  yaml = pkgs.formats.yaml { };
  schemaDirs = lib.unique (map (worker: toString worker.package.schema) workers);
  schemas = yaml.generate "boreal-local-schemas.yaml" { directories = schemaDirs; };
  policies = lib.mapAttrs (
    owner: user: yaml.generate "boreal-local-perms-${owner}.yaml" (userPolicy owner user)
  ) users;
  catalog = pkgs.runCommand "boreal-local-catalog" { } ''
    install -Dm444 ${schemas} "$out/schemas.yaml"
    ${lib.concatMapStringsSep "\n" (owner: ''
      install -Dm444 ${policies.${owner}} "$out/users/${owner}/perms.yaml"
    '') owners}
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
    ++ map (worker: worker.package) workers
    ++ map (owner: vaults.${owner}.package) vaultOwners;
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
assert lib.assertMsg (
  caller == null || builtins.hasAttr caller users
) "mkLocalOwners: caller must be declared in users";
assert lib.assertMsg (builtins.all (
  name: validName name && name != "root"
) owners) "mkLocal: invalid owner name";
assert lib.assertMsg (workers != [ ]) "mkLocal: configure at least one worker";
assert lib.assertMsg (builtins.all (
  { name, ... }: validName name && name != "vault" && !lib.hasSuffix "-vault" name
) workers) "mkLocal: invalid worker name or reserved vault queue";
assert lib.assertMsg (builtins.all (
  { package, ... }: package ? schema && validBin package && !(package.borealVault or false)
) workers) "mkLocal: workers must be executable buildRustWorker outputs with schemas";
assert lib.assertMsg (builtins.all
  ({ package, ... }: (package.borealVault or false) && validBin package)
  (builtins.attrValues vaults)
) "mkLocal: vault.package must be an executable buildRustVault output";
assert lib.assertMsg (
  caller == null
  || builtins.all (vault: (vault.secretDir or null) == null) (builtins.attrValues vaults)
) "mkLocalOwners: vault fixture directories are managed per owner; do not set vault.secretDir";
assert lib.assertMsg (
  secretDir == null
  || (
    builtins.isString secretDir
    && !builtins.hasContext secretDir
    && builtins.all safeComponent (lib.splitString "/" relativeDir)
  )
) "mkLocal: vault.secretDir must be a relative runtime path string with no traversal";
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
    local_caller=${lib.escapeShellArg (if caller == null then "" else caller)}
    owners=(${lib.escapeShellArgs owners})
    vault_owners=(${lib.escapeShellArgs vaultOwners})
    vault_bins=(${lib.escapeShellArgs (map (owner: binOf vaults.${owner}.package) vaultOwners)})
    vault_secret_rel=${lib.escapeShellArg relativeDir}
    worker_owners=(${lib.escapeShellArgs (map (worker: worker.owner) workers)})
    worker_names=(${lib.escapeShellArgs (map (worker: worker.name) workers)})
    worker_bins=(${lib.escapeShellArgs (map (worker: binOf worker.package) workers)})
  ''
  + builtins.readFile ./local.sh;
}
