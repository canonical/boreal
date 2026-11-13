{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.boreal.updater;
  schedule = pkgs.writeShellScriptBin "boreal-schedule-update" ''
    set -eu
    ${lib.getExe cfg.package} schedule "$@"
    exec ${pkgs.systemd}/bin/systemctl --no-block start boreal-updater.service
  '';
in
{
  options.boreal.updater = {
    enable = lib.mkEnableOption "the fail-closed Boreal update service";
    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../packages/updater.nix { };
      description = "The fail-closed Boreal updater package.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [
      cfg.package
      schedule
    ];
    systemd.services.boreal-updater = {
      description = "Prebuild a scheduled Boreal update (activation is blocked)";
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      path = [
        pkgs.nix
        pkgs.git
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
  };
}
