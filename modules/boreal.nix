{
  config,
  lib,
  pkgs,
  self ? null,
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
  reservedUsers = [
    "root"
    "admin"
    "temporal"
    "postgres"
    "boreal-broker"
    "sshd"
    "nobody"
  ];

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

  mkUserRc =
    user: userCfg:
    yamlFormat.generate (lib.strings.sanitizeDerivationName "boreal-rc-${user}.yaml") {
      edit_mode = userCfg.editMode;
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

  templateManifest = builtins.fromTOML (builtins.readFile ../templates/default/temporal/Cargo.toml);
  selfTemporalVersion =
    if
      self != null && (self ? outPath) && builtins.pathExists "${self.outPath}/temporal/Cargo.toml"
    then
      let
        manifest = builtins.fromTOML (builtins.readFile "${self.outPath}/temporal/Cargo.toml");
      in
      manifest.workspace.package.version or manifest.package.version or null
    else
      null;
  workerDerivedVersion = lib.findFirst (v: v != null && nonEmpty v) null (
    map (name: cfg.workerPackages.${name}.package.version or null) workerPackageNames
  );
  defaultLabVersion =
    if selfTemporalVersion != null && nonEmpty selfTemporalVersion then
      selfTemporalVersion
    else if workerDerivedVersion != null then
      workerDerivedVersion
    else
      templateManifest.workspace.package.version;

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

    version = mkOption {
      type = types.str;
      default = defaultLabVersion;
      defaultText = lib.literalExpression "temporal/Cargo.toml workspace.package.version";
      description = "Downstream lab version surfaced in boreal-shell (defaults to temporal/Cargo.toml).";
    };

    temporalAddress = mkOption {
      type = types.str;
      default = "127.0.0.1:7233";
      description = "Temporal frontend address configured on Boreal workers inside their isolated netns.";
    };

    shellPackage = mkOption {
      type = types.package;
      default = pkgs.callPackage ../packages/shell.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ../packages/shell.nix { }";
      description = "Restricted Boreal shell package assigned to lab user accounts.";
    };

    brokerPackage = mkOption {
      type = types.package;
      default = pkgs.callPackage ../packages/broker.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ../packages/broker.nix { }";
      description = "Boreal policy broker and worker gRPC gate package.";
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

            editMode = mkOption {
              type = types.enum [
                "emacs"
                "vi"
                "helix"
              ];
              default = "emacs";
              description = "Interactive line-editing mode written to ~/.borealrc for boreal-shell.";
            };

            permissions = {
              read = mkOption {
                type = permissionType;
                default = false;
                description = ''
                  Default read policy for this user's workers (`status`, `result`, `view`, and `list`).
                  `true` grants all Boreal users, `false` grants none beyond the owner,
                  and a list grants named Boreal users.
                '';
              };

              write = mkOption {
                type = permissionType;
                default = false;
                description = ''
                  Default write policy for this user's workers (`run` and `cancel`).
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

  config = lib.mkMerge [
    { _module.args.self = lib.mkOptionDefault null; }
    (mkIf cfg.enable {
      assertions = [
        {
          assertion = nonEmpty cfg.version && builtins.match "^[^[:cntrl:][:space:]]+$" cfg.version != null;
          message = "boreal.version must be a non-empty string without whitespace or control characters.";
        }
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
          assertion = lib.all (user: !(builtins.elem user reservedUsers) && validName user) userNames;
          message = "Every boreal.users account name must match ^[a-z][a-z0-9_-]*$ and must not be root or a reserved service account.";
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

      nix.gc.automatic = lib.mkForce false;
      boot.kernel.sysctl = lib.mkIf (!config.boot.isContainer) {
        "kernel.yama.ptrace_scope" = lib.mkDefault 2;
      };

      services.openssh = {
        allowSFTP = lib.mkForce false;
        settings = {
          PasswordAuthentication = lib.mkDefault false;
          KbdInteractiveAuthentication = lib.mkDefault false;
          PermitRootLogin = lib.mkForce "no";
          AllowTcpForwarding = lib.mkForce "no";
          AllowStreamLocalForwarding = lib.mkForce "no";
          AllowAgentForwarding = lib.mkForce "no";
          X11Forwarding = lib.mkForce false;
          PermitTunnel = lib.mkForce "no";
          PermitUserRC = lib.mkForce false;
          PermitUserEnvironment = lib.mkForce false;
        };
      };

      users.groups.boreal = { };

      users.users = lib.mapAttrs (_: userCfg: {
        isNormalUser = true;
        homeMode = "0700";
        shell = "${cfg.shellPackage}/bin/boreal-shell";
        extraGroups = [ "boreal" ];
        openssh.authorizedKeys.keys = userCfg.keys;
      }) cfg.users;

      systemd.tmpfiles.rules = [
        "d /etc/boreal 0750 root boreal -"
        "d /etc/boreal/users 0750 root boreal -"
        "d /run/netns 0700 root root -"
      ]
      ++ map (user: "d /etc/boreal/users/${user} 0750 root boreal -") userNames;

      environment.etc = {
        "boreal/version" = {
          text = "${cfg.version}\n";
          user = "root";
          group = "boreal";
          mode = "0440";
        };
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

      systemd.services =
        (lib.mapAttrs' (
          user: userCfg:
          let
            rcSource = mkUserRc user userCfg;
            homeDir = config.users.users.${user}.home;
          in
          lib.nameValuePair "boreal-user-rc-${user}" {
            description = "Provision ~/.borealrc for Boreal user ${user}";
            wantedBy = [ "multi-user.target" ];
            before = [ "sshd.service" ];
            after = [ "local-fs.target" ];
            restartTriggers = [ rcSource ];
            serviceConfig = {
              Type = "oneshot";
              RemainAfterExit = true;
              User = user;
              UMask = "0077";
              NoNewPrivileges = true;
              PrivateTmp = true;
              PrivateNetwork = true;
              ProtectSystem = "strict";
              ProtectProc = "invisible";
              ReadWritePaths = [ homeDir ];
              ExecStart = pkgs.writeShellScript "boreal-user-rc-${user}" ''
                set -eu
                home=${lib.escapeShellArg homeDir}
                tmp="$home/.borealrc.tmp.$$"
                ${pkgs.coreutils}/bin/rm -f -- "$tmp"
                ${pkgs.coreutils}/bin/install -m 0600 ${rcSource} "$tmp"
                ${pkgs.coreutils}/bin/mv -fT -- "$tmp" "$home/.borealrc"
              '';
            };
          }
        ) cfg.users)
        // builtins.listToAttrs (
          map (entry: {
            name = "boreal-worker-${entry.unitKey}";
            value = {
              description = "Boreal worker ${entry.unitKey}";
              wantedBy = [ "multi-user.target" ];
              after = [
                "boreal-netns-worker-${entry.unitKey}.service"
                "boreal-temporal-namespaces.service"
                "boreal-broker.service"
              ];
              requires = [ "boreal-netns-worker-${entry.unitKey}.service" ];
              wants = [
                "boreal-temporal-namespaces.service"
                "boreal-broker.service"
              ];
              environment = {
                TEMPORAL_ADDRESS = cfg.temporalAddress;
                TEMPORAL_NAMESPACE = entry.user;
                LAB_TASK_QUEUE = entry.unitKey;
              };
              serviceConfig = {
                User = entry.user;
                NetworkNamespacePath = "/run/netns/boreal-worker-${entry.unitKey}";
                ExecStart =
                  if builtins.hasAttr entry.cfg.package cfg.workerPackages then
                    lib.getExe cfg.workerPackages.${entry.cfg.package}.package
                  else
                    "/bin/false";
                Restart = "on-failure";
                RestartSec = lib.mkDefault 2;
                TimeoutStopSec = "infinity";
                NoNewPrivileges = true;
                PrivateTmp = true;
                ProtectSystem = "strict";
                ProtectProc = "invisible";
              };
            };
          }) workerEntries
        )
        // builtins.listToAttrs (
          lib.concatMap (
            entry:
            let
              pkg = cfg.workerPackages.${entry.cfg.package}.package or null;
            in
            if pkg == null || !(pkg ? scheduler) then
              [ ]
            else
              [
                {
                  name = "boreal-scheduler-${entry.unitKey}";
                  value = {
                    description = "Register Temporal schedules for ${entry.unitKey}";
                    wantedBy = [ "multi-user.target" ];
                    after = [
                      "boreal-netns-worker-${entry.unitKey}.service"
                      "boreal-temporal-namespaces.service"
                      "boreal-broker.service"
                    ];
                    requires = [ "boreal-netns-worker-${entry.unitKey}.service" ];
                    wants = [
                      "boreal-temporal-namespaces.service"
                      "boreal-broker.service"
                    ];
                    restartTriggers = [ pkg.scheduler ];
                    environment = {
                      TEMPORAL_ADDRESS = cfg.temporalAddress;
                      TEMPORAL_NAMESPACE = entry.user;
                      LAB_TASK_QUEUE = entry.unitKey;
                    };
                    serviceConfig = {
                      Type = "oneshot";
                      RemainAfterExit = true;
                      User = entry.user;
                      NetworkNamespacePath = "/run/netns/boreal-worker-${entry.unitKey}";
                      ExecStart = "${pkg.scheduler}/bin/scheduler";
                      Restart = "on-failure";
                      RestartSec = lib.mkDefault 2;
                      TimeoutStartSec = 60;
                      NoNewPrivileges = true;
                      PrivateTmp = true;
                      ProtectSystem = "strict";
                      ProtectProc = "invisible";
                    };
                  };
                }
              ]
          ) workerEntries
        );
    })
  ];
}
