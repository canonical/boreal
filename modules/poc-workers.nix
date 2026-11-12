{ config, lib, ... }:

let
  cfg = config.boreal.poc;
  inherit (lib) mkEnableOption mkIf mkOption types;
in
{
  options.boreal.poc = {
    enable = mkEnableOption "Boreal's insecure development worker configuration";

    acknowledgeInsecureTemporal = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Acknowledge that this prototype does not configure Temporal
        authentication or authorization and is unsuitable for a shared lab.
      '';
    };

    users = mkOption {
      default = { };
      description = "Demo Unix accounts and their public SSH keys.";
      type = types.attrsOf (types.submodule {
        options.sshKeys = mkOption {
          type = types.listOf types.str;
          default = [ ];
          description = "Public authorized keys; never put private keys here.";
        };
      });
    };

    workers = mkOption {
      default = { };
      description = "Explicitly configured demo workers.";
      type = types.attrsOf (types.submodule {
        options = {
          user = mkOption {
            type = types.str;
            description = "Name of an account declared in boreal.poc.users.";
          };
          package = mkOption {
            type = types.package;
            description = "Worker executable package.";
          };
          environment = mkOption {
            type = types.attrsOf types.str;
            default = { };
            description = "Non-secret worker configuration.";
          };
        };
      });
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.acknowledgeInsecureTemporal;
        message = "Boreal POC workers require acknowledgeInsecureTemporal = true; no Temporal access control is configured.";
      }
      {
        assertion = lib.all (worker: builtins.hasAttr worker.user cfg.users) (builtins.attrValues cfg.workers);
        message = "Every Boreal POC worker must belong to a user declared in boreal.poc.users.";
      }
    ];

    users.users = lib.mapAttrs (_: user: {
      isNormalUser = true;
      openssh.authorizedKeys.keys = user.sshKeys;
    }) cfg.users;

    systemd.services = lib.mapAttrs' (name: worker:
      lib.nameValuePair "boreal-worker-${name}" {
        description = "Boreal POC worker ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        environment = worker.environment;
        serviceConfig = {
          User = worker.user;
          ExecStart = lib.getExe worker.package;
          Restart = "on-failure";
          TimeoutStopSec = "infinity";
        };
      }) cfg.workers;
  };
}
