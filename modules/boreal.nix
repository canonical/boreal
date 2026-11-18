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
      package = workerCfg.package.outputName;
      permissions = resolveWorkerPermissions userCfg workerCfg;
    }) userCfg.workers;
  };

  mkUserRc =
    user: userCfg:
    yamlFormat.generate (lib.strings.sanitizeDerivationName "boreal-rc-${user}.yaml") {
      edit_mode = userCfg.editMode;
    };

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
    map (entry: entry.cfg.package.version or null) workerEntries
  );
  schemaOf = pkg: pkg.schema or null;
  schemaDirs = lib.unique (
    lib.filter (dir: dir != null) (map (entry: schemaOf entry.cfg.package) workerEntries)
  );
  outputGroups = lib.groupBy (entry: entry.cfg.package.outputName or "out") workerEntries;
  outputNameCollision = lib.any (
    entries:
    let
      paths = lib.unique (
        lib.filter (path: path != null) (
          map (
            entry:
            let
              schema = schemaOf entry.cfg.package;
            in
            if schema == null then null else schema.outPath
          ) entries
        )
      );
    in
    builtins.length paths > 1
  ) (builtins.attrValues outputGroups);
  binOf = pkg: "${pkg}/bin/${pkg.outputName}";
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

in
{
  imports = [
    (lib.mkRemovedOptionModule [
      "boreal"
      "poc"
    ] "Use boreal.enable, boreal.users, and boreal.updater instead.")
    (lib.mkRemovedOptionModule
      [
        "boreal"
        "workerPackages"
      ]
      "Set boreal.users.<user>.workers.<name>.package to a worker output. Its schema output is listed in /etc/boreal/schemas.yaml."
    )
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
                      type = types.package;
                      description = "Worker executable output. The bin name is the output name, and the sibling schema output supplies schema.yaml.";
                    };

                    schedule = {
                      enable = mkOption {
                        type = types.bool;
                        default = false;
                        description = "Start boreal-schedule-<user>-<worker> with schedule.package.";
                      };

                      package = mkOption {
                        type = types.nullOr types.package;
                        default = null;
                        description = "Schedule executable output. Required when enable is true. The bin name is the output name.";
                      };
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
          assertion = lib.all (entry: schemaOf entry.cfg.package != null) workerEntries;
          message = "Every Boreal worker package must have a schema output containing schema.yaml.";
        }
        {
          assertion = !outputNameCollision;
          message = "Worker output names must be unique across different schema outputs.";
        }
        {
          assertion = lib.all (
            entry: !entry.cfg.schedule.enable || entry.cfg.schedule.package != null
          ) workerEntries;
          message = "boreal.users.<user>.workers.<name>.schedule.package is required when schedule.enable is true.";
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
        "boreal/schemas.yaml" = {
          source = yamlFormat.generate "boreal-schemas.yaml" {
            directories = map toString schemaDirs;
          };
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
              };
              serviceConfig = {
                User = entry.user;
                NetworkNamespacePath = "/run/netns/boreal-worker-${entry.unitKey}";
                ExecStart = "${binOf entry.cfg.package} ${entry.worker}";
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
              pkg = entry.cfg.schedule.package;
            in
            if !entry.cfg.schedule.enable || pkg == null then
              [ ]
            else
              [
                {
                  name = "boreal-schedule-${entry.unitKey}";
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
                    restartTriggers = [ pkg ];
                    environment = {
                      TEMPORAL_ADDRESS = cfg.temporalAddress;
                      TEMPORAL_NAMESPACE = entry.user;
                    };
                    serviceConfig = {
                      Type = "oneshot";
                      RemainAfterExit = true;
                      User = entry.user;
                      NetworkNamespacePath = "/run/netns/boreal-worker-${entry.unitKey}";
                      ExecStart = "${binOf pkg} ${entry.worker}";
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
