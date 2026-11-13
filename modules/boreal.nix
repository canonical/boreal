{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.boreal;
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    ;

  borealPackages = import ../packages { inherit pkgs; };
  workerPackages = {
    inherit (borealPackages) lab-worker lab-worker-alt;
  };

  yamlFormat = pkgs.formats.yaml { };
  permissionType = types.either types.bool (types.listOf types.str);
  validName = name: builtins.match "^[a-z][a-z0-9_-]*$" name != null;

  normalizeGrant = grant: if builtins.isList grant then lib.unique grant else grant;
  grantUsers = grant: if builtins.isList grant then grant else [ ];

  resolveWorkerPermissions = userCfg: workerCfg: {
    read = normalizeGrant (
      if workerCfg.permissions.read != null then workerCfg.permissions.read else userCfg.permissions.read
    );
    write = normalizeGrant (
      if workerCfg.permissions.write != null then
        workerCfg.permissions.write
      else
        userCfg.permissions.write
    );
  };

  mkUserPolicy = user: userCfg: {
    inherit user;
    permissions = {
      read = normalizeGrant userCfg.permissions.read;
      write = normalizeGrant userCfg.permissions.write;
    };
    workers = lib.mapAttrs (_: workerCfg: {
      inherit (workerCfg) package;
      permissions = resolveWorkerPermissions userCfg workerCfg;
    }) userCfg.workers;
  };

  userNames = builtins.attrNames cfg.users;
  workerEntries = lib.concatLists (
    lib.mapAttrsToList (
      user: userCfg:
      lib.mapAttrsToList (worker: workerCfg: {
        inherit user worker;
        unitKey = "${user}-${worker}";
        cfg = workerCfg;
      }) userCfg.workers
    ) cfg.users
  );
  unitKeys = map (entry: entry.unitKey) workerEntries;

  referencedUsers = lib.concatLists (
    lib.mapAttrsToList (
      _: userCfg:
      grantUsers userCfg.permissions.read
      ++ grantUsers userCfg.permissions.write
      ++ lib.concatLists (
        lib.mapAttrsToList (
          _: workerCfg: grantUsers workerCfg.permissions.read ++ grantUsers workerCfg.permissions.write
        ) userCfg.workers
      )
    ) cfg.users
  );
in
{
  imports = [
    (lib.mkRemovedOptionModule [
      "boreal"
      "poc"
    ] "Use boreal.enable, boreal.users, and boreal.updater instead.")
  ];

  options.boreal = {
    enable = mkEnableOption "Boreal lab users, workers, and permission policies";

    temporalAddress = mkOption {
      type = types.str;
      default = "127.0.0.1:7233";
      description = "Temporal frontend address configured on Boreal workers.";
    };

    users = mkOption {
      default = { };
      description = "Boreal lab accounts, SSH public keys, permissions, and workers.";
      type = types.attrsOf (
        types.submodule {
          options = {
            keys = mkOption {
              type = types.listOf types.str;
              default = [ ];
              description = "Public SSH authorized keys; never place private keys here.";
            };

            permissions = {
              read = mkOption {
                type = permissionType;
                default = false;
                description = ''
                  Default read policy for this user's workers (`status` and `result`).
                  `true` grants all Boreal users, `false` grants none beyond the owner,
                  and a list grants named Boreal users.
                '';
              };

              write = mkOption {
                type = permissionType;
                default = false;
                description = ''
                  Default write policy for this user's workers (`start` and `cancel`).
                  `true` grants all Boreal users, `false` grants none beyond the owner,
                  and a list grants named Boreal users.
                '';
              };
            };

            workers = mkOption {
              default = { };
              description = "Boreal Temporal workers owned by this user.";
              type = types.attrsOf (
                types.submodule {
                  options = {
                    package = mkOption {
                      type = types.str;
                      description = "Name of a known Boreal worker package (such as lab-worker or lab-worker-alt).";
                    };

                    permissions = {
                      read = mkOption {
                        type = types.nullOr permissionType;
                        default = null;
                        description = "Worker-specific read policy; replaces the user's default read setting when set.";
                      };

                      write = mkOption {
                        type = types.nullOr permissionType;
                        default = null;
                        description = "Worker-specific write policy; replaces the user's default write setting when set.";
                      };
                    };
                  };
                }
              );
            };
          };
        }
      );
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = builtins.match "^[[:space:]]*$" cfg.temporalAddress == null;
        message = "boreal.temporalAddress must not be empty.";
      }
      {
        assertion = lib.all (user: user != "root" && validName user) userNames;
        message = "Every boreal.users account name must match ^[a-z][a-z0-9_-]*$ and must not be root.";
      }
      {
        assertion = lib.all (entry: validName entry.worker) workerEntries;
        message = "Every Boreal worker name must match ^[a-z][a-z0-9_-]*$.";
      }
      {
        assertion = builtins.length unitKeys == builtins.length (lib.unique unitKeys);
        message = "Boreal user-worker identifiers (<user>-<worker>) must be unique across boreal.users.";
      }
      {
        assertion = lib.all (entry: builtins.hasAttr entry.cfg.package workerPackages) workerEntries;
        message = "Every Boreal worker package must be one of the known Boreal worker packages: ${lib.concatStringsSep ", " (builtins.attrNames workerPackages)}.";
      }
      {
        assertion = lib.all (target: builtins.hasAttr target cfg.users) referencedUsers;
        message = "Every user granted access in Boreal permissions must be declared in boreal.users.";
      }
    ];

    users.groups.boreal = { };

    users.users = lib.mapAttrs (_: userCfg: {
      isNormalUser = true;
      extraGroups = [ "boreal" ];
      openssh.authorizedKeys.keys = userCfg.keys;
    }) cfg.users;

    systemd.tmpfiles.rules = [
      "d /etc/boreal 0750 root boreal -"
      "d /etc/boreal/users 0750 root boreal -"
    ]
    ++ map (user: "d /etc/boreal/users/${user} 0750 root boreal -") userNames;

    environment.etc = lib.mapAttrs' (
      user: userCfg:
      lib.nameValuePair "boreal/users/${user}/perms.yaml" {
        source = yamlFormat.generate (lib.strings.sanitizeDerivationName "boreal-perms-${user}.yaml") (
          mkUserPolicy user userCfg
        );
        user = "root";
        group = "boreal";
        mode = "0440";
      }
    ) cfg.users;

    systemd.services = builtins.listToAttrs (
      map (entry: {
        name = "boreal-worker-${entry.unitKey}";
        value = {
          description = "Boreal worker ${entry.unitKey}";
          wantedBy = [ "multi-user.target" ];
          after = [ "network-online.target" ];
          wants = [ "network-online.target" ];
          environment = {
            TEMPORAL_ADDRESS = cfg.temporalAddress;
            TEMPORAL_NAMESPACE = entry.user;
            LAB_TASK_QUEUE = entry.unitKey;
          };
          serviceConfig = {
            User = entry.user;
            ExecStart =
              if builtins.hasAttr entry.cfg.package workerPackages then
                lib.getExe workerPackages.${entry.cfg.package}
              else
                "/bin/false";
            Restart = "on-failure";
            TimeoutStopSec = "infinity";
          };
        };
      }) workerEntries
    );
  };
}
