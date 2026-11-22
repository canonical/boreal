{ config, lib, ... }:

let
  cfg = config.boreal;
  inherit (cfg) artifacts;
  storeType = lib.types.addCheck lib.types.str (
    value:
    builtins.match "(file:///[^[:space:]]+|https://[^/@?[:space:]]+([/?][^[:space:]]*)?)" value != null
  );
  storeOption =
    description:
    lib.mkOption {
      inherit description;
      type = lib.types.nullOr storeType;
      default = null;
    };
  enabledWorkers = lib.concatLists (
    lib.mapAttrsToList (
      owner: user:
      lib.mapAttrsToList (worker: selected: {
        key = "${owner}-${worker}";
        inherit selected;
      }) (lib.filterAttrs (_: worker: worker.artifacts != "off" && placed worker.placement) user.workers)
    ) cfg.users
  );
  placed =
    placement:
    if cfg.role == "central" then
      placement.central
    else
      builtins.elem config.networking.hostName placement.peripherals;
in
{
  options.boreal.artifacts = {
    readStore = storeOption "Worker-visible Nix binary cache URL for reads (file:// or HTTPS; no secrets).";
    writeStore = storeOption "Worker-visible Nix binary cache URL for uploads (file:// or HTTPS; no secrets).";
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = enabledWorkers == [ ] || artifacts.readStore != null;
        message = "Boreal workers using artifacts require boreal.artifacts.readStore on this host.";
      }
      {
        assertion =
          artifacts.writeStore != null
          || lib.all (entry: entry.selected.artifacts != "publish") enabledWorkers;
        message = "Boreal artifact publishers require boreal.artifacts.writeStore on this host.";
      }
      {
        assertion = enabledWorkers == [ ] || config.nix.enable;
        message = "Boreal artifact workers require the host's Nix store and daemon.";
      }
    ];

    systemd.services = builtins.listToAttrs (
      map (
        entry:
        let
          runtime = "boreal-artifacts-${entry.key}";
        in
        lib.nameValuePair "boreal-worker-${entry.key}" {
          path = [ config.nix.package ];
          after = [ "nix-daemon.socket" ];
          requires = [ "nix-daemon.socket" ];
          environment = {
            BOREAL_ARTIFACT_RUNTIME_DIR = "%t/${runtime}";
          }
          // lib.optionalAttrs (artifacts.readStore != null) {
            BOREAL_ARTIFACT_READ_STORE = artifacts.readStore;
          }
          // lib.optionalAttrs (entry.selected.artifacts == "publish" && artifacts.writeStore != null) {
            BOREAL_ARTIFACT_WRITE_STORE = artifacts.writeStore;
          };
          serviceConfig = {
            RuntimeDirectory = runtime;
            RuntimeDirectoryMode = "0700";
          };
        }
      ) enabledWorkers
    );
  };
}
