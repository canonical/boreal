{
  config,
  lib,
  pkgs,
  borealRustPlatform,
  ...
}:

let
  borealCfg = config.boreal;
  cfg = borealCfg.admin;
  yamlFormat = pkgs.formats.yaml { };
  mkShellRc = import ./shell-rc.nix { inherit lib pkgs; };

  workerUnits = lib.concatLists (
    lib.mapAttrsToList (
      user: userCfg:
      lib.concatLists (
        lib.mapAttrsToList (
          worker: workerCfg:
          lib.optional workerCfg.placement.central "boreal-worker-${user}-${worker}.service"
        ) userCfg.workers
      )
      ++ lib.optional (userCfg.vault != null) "boreal-worker-${user}-vault.service"
    ) borealCfg.users
  );

  allowedServices = [ "temporal.service" ] ++ workerUnits;

  adminPolicy = {
    services = allowedServices;
    updaterEnabled = borealCfg.updater.enable;
    allowPoweroff = cfg.allowPoweroff;
    ctlBin = "${cfg.package}/bin/boreal-admin-ctl";
  }
  // lib.optionalAttrs borealCfg.updater.enable {
    updaterBin = lib.getExe borealCfg.updater.package;
  };
in
{
  options.boreal.admin = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = borealCfg.enable && borealCfg.role == "central";
      defaultText = lib.literalExpression ''config.boreal.enable && config.boreal.role == "central"'';
      description = "Enable the restricted Boreal admin account and root helper.";
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../packages/admin.nix { rustPlatform = borealRustPlatform; };
      defaultText = lib.literalExpression "pkgs.callPackage ../packages/admin.nix { rustPlatform = borealRustPlatform; }";
      description = "Package providing boreal-admin and boreal-admin-ctl.";
    };

    keys = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Public SSH keys authorized for the restricted admin account.";
    };

    allowPoweroff = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Allow boreal-admin to power off the system (intended for disposable VMs).";
    };

    editMode = lib.mkOption {
      type = lib.types.enum [
        "emacs"
        "vi"
        "helix"
      ];
      default = "emacs";
      description = "Interactive line-editing mode written to the admin account's ~/.borealrc.";
    };
  };

  config = lib.mkIf (borealCfg.enable && borealCfg.role == "central" && cfg.enable) {
    users.groups.boreal-admin = { };

    users.users = {
      root.hashedPassword = lib.mkDefault "!";
      admin = {
        isNormalUser = true;
        homeMode = "0700";
        shell = "${cfg.package}/bin/boreal-admin";
        extraGroups = [ "boreal-admin" ];
        openssh.authorizedKeys.keys = cfg.keys;
      };
    };

    systemd.tmpfiles.rules = [
      "d /etc/boreal-admin 0750 root boreal-admin -"
    ];

    systemd.services.boreal-user-rc-admin =
      mkShellRc "admin" config.users.users.admin.home
        cfg.editMode;

    environment.etc."boreal-admin/policy.yaml" = {
      source = yamlFormat.generate "boreal-admin-policy.yaml" adminPolicy;
      user = "root";
      group = "boreal-admin";
      mode = "0440";
    };

    security.sudo = {
      enable = true;
      wheelNeedsPassword = lib.mkForce true;
      extraRules = [
        {
          users = [ "admin" ];
          commands = [
            {
              command = "${cfg.package}/bin/boreal-admin-ctl";
              options = [ "NOPASSWD" ];
            }
          ];
        }
      ];
    };
  };
}
