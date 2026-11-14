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

  yamlFormat = pkgs.formats.yaml { };
  permissionType = types.either types.bool (types.listOf types.str);
  nonEmpty = text: builtins.match "^[[:space:]]*$" text == null;
  validName = name: builtins.match "^[a-z][a-z0-9_-]*$" name != null;
  validArgName = name: builtins.match "^[a-z][a-z0-9_]*$" name != null;

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

  mkArgSpec =
    arg:
    {
      inherit (arg) name type description;
    }
    // lib.optionalAttrs (arg.type == "enum") {
      inherit (arg) choices;
    }
    // lib.optionalAttrs (arg.min != null) {
      inherit (arg) min;
    }
    // lib.optionalAttrs (arg.max != null) {
      inherit (arg) max;
    }
    // lib.optionalAttrs (arg.default != null) {
      inherit (arg) default;
    };

  mkWorkersCatalog = lib.mapAttrs (_: pkgCfg: {
    inherit (pkgCfg) workflowType description;
    args = map mkArgSpec pkgCfg.args;
  }) cfg.workerPackages;

  validArgOrder =
    args:
    (builtins.foldl'
      (
        state: arg:
        let
          isOptional = arg.default != null;
        in
        {
          seenOptional = state.seenOptional || isOptional;
          ok = state.ok && !(state.seenOptional && !isOptional);
        }
      )
      {
        seenOptional = false;
        ok = true;
      }
      args
    ).ok;

  validArgBounds =
    arg:
    let
      isNumeric = arg.type == "int" || arg.type == "float";
      boundsAllowed = if isNumeric then true else arg.min == null && arg.max == null;
      intBoundsValid =
        arg.type != "int"
        || ((arg.min == null || builtins.isInt arg.min) && (arg.max == null || builtins.isInt arg.max));
      orderValid = arg.min == null || arg.max == null || arg.min <= arg.max;
    in
    boundsAllowed && intBoundsValid && orderValid;

  validArgChoices =
    arg:
    if arg.type == "enum" then
      arg.choices != [ ]
      && lib.all nonEmpty arg.choices
      && builtins.length arg.choices == builtins.length (lib.unique arg.choices)
    else
      arg.choices == [ ];

  validArgDefault =
    arg:
    if arg.default == null then
      true
    else
      let
        typeMatches =
          if arg.type == "string" then
            builtins.isString arg.default
          else if arg.type == "enum" then
            builtins.isString arg.default && builtins.elem arg.default arg.choices
          else if arg.type == "int" then
            builtins.isInt arg.default
          else if arg.type == "float" then
            builtins.isInt arg.default || builtins.isFloat arg.default
          else
            builtins.isBool arg.default;
        withinBounds =
          !(builtins.isInt arg.default || builtins.isFloat arg.default)
          || ((arg.min == null || arg.default >= arg.min) && (arg.max == null || arg.default <= arg.max));
      in
      typeMatches && withinBounds;

  workerPackageNames = builtins.attrNames cfg.workerPackages;
  workerPackageValues = builtins.attrValues cfg.workerPackages;

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

  argModule = types.submodule {
    options = {
      name = mkOption {
        type = types.str;
        description = "Workflow input field and shell argument name.";
      };

      type = mkOption {
        type = types.enum [
          "string"
          "int"
          "float"
          "bool"
          "enum"
        ];
        description = "Scalar argument type used for validation and JSON encoding.";
      };

      description = mkOption {
        type = types.str;
        default = "";
        description = "Short human-readable argument description.";
      };

      choices = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Allowed values when type is enum.";
      };

      min = mkOption {
        type = types.nullOr types.number;
        default = null;
        description = "Inclusive lower bound for int or float arguments.";
      };

      max = mkOption {
        type = types.nullOr types.number;
        default = null;
        description = "Inclusive upper bound for int or float arguments.";
      };

      default = mkOption {
        type = types.nullOr (
          types.oneOf [
            types.str
            types.number
            types.bool
          ]
        );
        default = null;
        description = "Optional default value; arguments with null default are required.";
      };
    };
  };
in
{
  imports = [
    (lib.mkRemovedOptionModule [
      "boreal"
      "poc"
    ] "Use boreal.enable, boreal.workerPackages, boreal.users, and boreal.updater instead.")
  ];

  options.boreal = {
    enable = mkEnableOption "Boreal lab users, workers, and permission policies";

    temporalAddress = mkOption {
      type = types.str;
      default = "127.0.0.1:7233";
      description = "Temporal frontend address configured on Boreal workers.";
    };

    workerPackages = mkOption {
      default = { };
      description = "Registered Boreal worker packages and their workflow schemas.";
      type = types.attrsOf (
        types.submodule {
          options = {
            package = mkOption {
              type = types.package;
              description = "Executable package running this Temporal worker.";
            };

            workflowType = mkOption {
              type = types.str;
              description = "Registered Temporal workflow type name.";
            };

            description = mkOption {
              type = types.str;
              default = "";
              description = "Short summary shown in shell listings and completions.";
            };

            args = mkOption {
              type = types.listOf argModule;
              default = [ ];
              description = "Ordered flat input arguments serialized as a JSON object.";
            };
          };
        }
      );
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
                      description = "Name of a worker registered in boreal.workerPackages.";
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
        assertion = nonEmpty cfg.temporalAddress;
        message = "boreal.temporalAddress must not be empty.";
      }
      {
        assertion = lib.all validName workerPackageNames;
        message = "Every boreal.workerPackages key must match ^[a-z][a-z0-9_-]*$.";
      }
      {
        assertion = lib.all (pkgCfg: nonEmpty pkgCfg.workflowType) workerPackageValues;
        message = "Every boreal.workerPackages entry must declare a non-empty workflowType.";
      }
      {
        assertion = lib.all (
          pkgCfg:
          let
            argNames = map (arg: arg.name) pkgCfg.args;
          in
          lib.all validArgName argNames && builtins.length argNames == builtins.length (lib.unique argNames)
        ) workerPackageValues;
        message = "Worker argument names must be unique per package and match ^[a-z][a-z0-9_]*$.";
      }
      {
        assertion = lib.all (pkgCfg: validArgOrder pkgCfg.args) workerPackageValues;
        message = "Required worker arguments (default = null) must precede optional arguments.";
      }
      {
        assertion = lib.all (pkgCfg: lib.all validArgChoices pkgCfg.args) workerPackageValues;
        message = "Enum worker arguments require unique non-empty choices; non-enum arguments must omit choices.";
      }
      {
        assertion = lib.all (pkgCfg: lib.all validArgBounds pkgCfg.args) workerPackageValues;
        message = "Worker argument min/max bounds are valid only on int/float types with min <= max.";
      }
      {
        assertion = lib.all (pkgCfg: lib.all validArgDefault pkgCfg.args) workerPackageValues;
        message = "Worker argument default values must match their declared type, choices, and min/max bounds.";
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
        assertion = lib.all (entry: builtins.hasAttr entry.cfg.package cfg.workerPackages) workerEntries;
        message = "Every Boreal user worker package must be declared in boreal.workerPackages.";
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

    environment.etc = {
      "boreal/workers.yaml" = {
        source = yamlFormat.generate "boreal-workers.yaml" mkWorkersCatalog;
        user = "root";
        group = "boreal";
        mode = "0440";
      };
    }
    // lib.mapAttrs' (
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
              if builtins.hasAttr entry.cfg.package cfg.workerPackages then
                lib.getExe cfg.workerPackages.${entry.cfg.package}.package
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
