{
  config,
  lib,
  pkgs,
  borealRustPlatform,
  ...
}:

let
  borealCfg = config.boreal;
  cfg = borealCfg.updater;
  yamlFormat = pkgs.formats.yaml { };

  validHost =
    host:
    host != ""
    && builtins.stringLength host <= 63
    && builtins.match "^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?$" host != null;

  workerUnits = lib.concatLists (
    lib.mapAttrsToList (
      user: userCfg:
      lib.concatLists (
        lib.mapAttrsToList (
          worker: workerCfg:
          lib.optional workerCfg.placement.central "boreal-worker-${user}-${worker}.service"
          ++ lib.optional workerCfg.schedule.enable "boreal-schedule-${user}-${worker}.service"
        ) userCfg.workers
      )
      ++ lib.optional (userCfg.vault != null) "boreal-worker-${user}-vault.service"
    ) borealCfg.users
  );

  updaterPolicy = {
    projectDir = "/etc/nixos";
    host = config.networking.hostName;
    allowSwitch = config.system.switch.enable;
    inherit workerUnits;
    brokerSocket = "/run/boreal/updater.sock";
  };

  removedMsg = "Place the downstream Git checkout at /etc/nixos (tracking an https:// remote branch) and set networking.hostName instead.";
in
{
  imports = [
    (lib.mkRemovedOptionModule [
      "boreal"
      "updater"
      "repository"
    ] removedMsg)
    (lib.mkRemovedOptionModule [
      "boreal"
      "updater"
      "branch"
    ] removedMsg)
    (lib.mkRemovedOptionModule [
      "boreal"
      "updater"
      "configurationName"
    ] removedMsg)
  ];

  options.boreal.updater = {
    enable = lib.mkEnableOption "the gated Boreal update service";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../packages/updater.nix { rustPlatform = borealRustPlatform; };
      defaultText = lib.literalExpression "pkgs.callPackage ../packages/updater.nix { rustPlatform = borealRustPlatform; }";
      description = "The Boreal updater package.";
    };
  };

  config = lib.mkIf (cfg.enable && borealCfg.role == "central") {
    assertions = [
      {
        assertion = validHost config.networking.hostName;
        message = "networking.hostName must be a single DNS-style hostname when boreal.updater is enabled.";
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
      pkgs.git
    ];

    systemd.services = {
      boreal-updater = {
        description = "Drain running pipelines and activate a prebuilt Boreal generation";
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
