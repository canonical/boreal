{
  config,
  lib,
  pkgs,
  ...
}:

let
  borealCfg = config.boreal;
  cfg = borealCfg.updater;
  yamlFormat = pkgs.formats.yaml { };

  validRepo =
    repo:
    builtins.match "^https://[A-Za-z0-9.-]+(:[0-9]+)?/[A-Za-z0-9/._~-]+$" repo != null
    && !(lib.hasInfix ".." repo);

  validBranch =
    branch:
    branch != ""
    && builtins.stringLength branch <= 200
    && !(lib.hasPrefix "/" branch)
    && !(lib.hasSuffix "/" branch)
    && !(lib.hasSuffix ".lock" branch)
    && !(lib.hasInfix ".." branch)
    && !(lib.hasInfix "//" branch)
    && builtins.match "^[A-Za-z0-9/_.-]+$" branch != null;

  validHost =
    host:
    host != ""
    && builtins.stringLength host <= 63
    && builtins.match "^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?$" host != null;

  workerUnits = lib.concatLists (
    lib.mapAttrsToList (
      user: userCfg:
      map (worker: "boreal-worker-${user}-${worker}.service") (builtins.attrNames userCfg.workers)
    ) borealCfg.users
  );

  updaterPolicy = {
    inherit (cfg) repository;
    host = cfg.configurationName;
    allowSwitch = config.system.switch.enable;
    inherit workerUnits;
    brokerSocket = "/run/boreal/updater.sock";
  }
  // lib.optionalAttrs (cfg.branch != null) {
    inherit (cfg) branch;
  };

  schedule = pkgs.writeShellScriptBin "boreal-schedule-update" ''
    set -eu
    ${lib.getExe cfg.package} schedule "$@"
    exec ${config.systemd.package}/bin/systemctl --no-block start boreal-updater.service
  '';
in
{
  options.boreal.updater = {
    enable = lib.mkEnableOption "the gated Boreal update service";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../packages/updater.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ../packages/updater.nix { }";
      description = "The Boreal updater package.";
    };

    repository = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Downstream lab HTTPS Git repository URL permitted for updates.";
    };

    branch = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Reviewed Git branch required on scheduled downstream commits when set.";
    };

    configurationName = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
      defaultText = lib.literalExpression "config.networking.hostName";
      description = "nixosConfigurations.<name> attribute built and activated for this host.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = validRepo cfg.repository;
        message = "boreal.updater.repository must be an https:// Git repository URL without credentials, query, or fragment.";
      }
      {
        assertion = cfg.branch == null || validBranch cfg.branch;
        message = "boreal.updater.branch must be null or a valid reviewed Git branch name.";
      }
      {
        assertion = validHost cfg.configurationName;
        message = "boreal.updater.configurationName must be a single DNS-style hostname.";
      }
    ];

    environment.etc."boreal/updater.yaml" = {
      source = yamlFormat.generate "boreal-updater.yaml" updaterPolicy;
      user = "root";
      group = "root";
      mode = "0400";
    };

    environment.systemPackages = [
      cfg.package
      schedule
    ];

    systemd.services = {
      boreal-updater = {
        description = "Prebuild, drain, and activate a scheduled Boreal update";
        wants = [ "network-online.target" ];
        after = [
          "network-online.target"
          "temporal.service"
          "boreal-broker.service"
        ];
        restartIfChanged = false;
        stopIfChanged = false;
        path = [
          pkgs.nix
          pkgs.git
          config.systemd.package
        ];
        serviceConfig = {
          Type = "oneshot";
          User = "root";
          StateDirectory = "boreal-updater";
          StateDirectoryMode = "0700";
          TimeoutStartSec = "infinity";
          ExecStart = "${lib.getExe cfg.package} run";
        };
      };

      boreal-updater-recover = {
        description = "Reconcile interrupted Boreal update state on boot";
        wantedBy = [ "multi-user.target" ];
        after = [
          "temporal.service"
          "boreal-broker.service"
        ];
        wants = [
          "temporal.service"
          "boreal-broker.service"
        ];
        restartIfChanged = false;
        stopIfChanged = false;
        path = [ config.systemd.package ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          User = "root";
          StateDirectory = "boreal-updater";
          StateDirectoryMode = "0700";
          ExecStart = "${lib.getExe cfg.package} recover";
        };
      };
    };
  };
}
