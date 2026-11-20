{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.boreal;
  vaults = lib.filterAttrs (_: user: user.vault != null) cfg.users;
  policy = (pkgs.formats.json { }).generate "boreal-vaults.json" (
    lib.mapAttrs (_: user: user.vault.grants) vaults
  );
  validOperation = op: builtins.match "[A-Za-z0-9_.:-]{1,128}" op != null;
in
{
  config = lib.mkIf (cfg.enable && cfg.role == "central" && vaults != { }) {
    assertions = lib.mapAttrsToList (owner: user: {
      assertion =
        (user.vault.package.borealVault or false)
        && user.vault.package.outputName != "out"
        && user.vault.grants != { }
        && lib.all (name: builtins.hasAttr name user.workers) (builtins.attrNames user.vault.grants)
        && lib.all (ops: ops != [ ] && lib.all validOperation ops) (builtins.attrValues user.vault.grants);
      message = "Vault for ${owner} requires a buildRustVault executable output and grants for declared workers with exact activity names.";
    }) vaults;

    environment.etc."boreal/vaults.json" = {
      source = policy;
      mode = "0400";
      user = "root";
      group = "root";
    };

    systemd.services = {
      boreal-broker = {
        restartTriggers = [ policy ];
        environment.BOREAL_VAULT_CONFIG = "/etc/boreal/vaults.json";
      };
    }
    // lib.mapAttrs' (
      owner: user:
      let
        key = "${owner}-vault";
        netns = "boreal-netns-worker-${key}.service";
        inherit (user.vault) package;
      in
      lib.nameValuePair "boreal-worker-${key}" {
        description = "Boreal central activity vault for ${owner}";
        wantedBy = [ "multi-user.target" ];
        after = [
          netns
          "boreal-broker.service"
        ];
        requires = [ netns ];
        wants = [ "boreal-broker.service" ];
        environment = {
          TEMPORAL_ADDRESS = cfg.temporalAddress;
          TEMPORAL_NAMESPACE = owner;
        };
        serviceConfig = {
          User = owner;
          NetworkNamespacePath = "/run/netns/boreal-worker-${key}";
          ExecStart = "${package}/bin/${package.outputName}";
          Restart = "on-failure";
          RestartSec = 2;
          TimeoutStopSec = "infinity";
          NoNewPrivileges = true;
          PrivateTmp = true;
          ProtectSystem = "strict";
          ProtectProc = "invisible";
          UMask = "0077";
        };
      }
    ) vaults;
  };
}
