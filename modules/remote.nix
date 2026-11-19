{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.boreal;
  inherit (lib)
    mkIf
    mkMerge
    mkOption
    types
    ;
  inherit (config.networking) hostName;

  runtimePath = types.addCheck types.str (
    value:
    !builtins.hasContext value
    && builtins.match "/[A-Za-z0-9._/-]+" value != null
    && !(lib.hasPrefix "/nix/store/" value || value == "/nix/store")
    && lib.all (part: part != "" && part != "." && part != "..") (
      lib.splitString "/" (lib.removePrefix "/" value)
    )
  );
  validHost = name: builtins.match "[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?" name != null;
  endpoint = types.addCheck types.str (
    value:
    let
      parts = builtins.match "([A-Za-z0-9._-]+|[[][0-9A-Fa-f:.]+[]]):([1-9][0-9]{0,4})" value;
    in
    parts != null && builtins.fromJSON (builtins.elemAt parts 1) <= 65535
  );
  listen = types.addCheck endpoint (
    value:
    let
      parts = builtins.match "([0-9]+)[.]([0-9]+)[.]([0-9]+)[.]([0-9]+):[0-9]+" value;
      octet = part: builtins.match "(0|[1-9][0-9]{0,2})" part != null && builtins.fromJSON part <= 255;
    in
    (parts != null && lib.all octet parts)
    || builtins.match "[[][0-9A-Fa-f:.]*:[0-9A-Fa-f:.]*[]]:[0-9]+" value != null
  );
  pathOption =
    description:
    mkOption {
      type = runtimePath;
      inherit description;
    };
  endpointOption =
    description:
    mkOption {
      type = endpoint;
      inherit description;
    };

  workers = lib.concatLists (
    lib.mapAttrsToList (
      owner: user:
      lib.mapAttrsToList (worker: settings: {
        inherit owner worker settings;
        key = "${owner}-${worker}";
      }) user.workers
    ) cfg.users
  );
  remoteWorkers = lib.filter (entry: entry.settings.placement.peripherals != [ ]) workers;
  assigned = lib.filter (
    entry: builtins.elem hostName entry.settings.placement.peripherals
  ) remoteWorkers;
  owners = lib.unique (map (entry: entry.owner) assigned);
  ingress = cfg.remote.central;
  client = cfg.remote.peripheral;

  ingressPolicy = (pkgs.formats.json { }).generate "boreal-remote-workers.json" {
    listen = ingress.listenAddress;
    certificate = ingress.serverCertificate;
    privateKey = ingress.serverKey;
    clientCa = ingress.clientCaCertificate;
    replicas = lib.concatMap (
      entry:
      map (host: {
        inherit (entry) owner worker;
        certificate = "${ingress.clientCertificatesDirectory}/${entry.key}@${host}.pem";
      }) entry.settings.placement.peripherals
    ) remoteWorkers;
  };
in
{
  options.boreal.remote = {
    central = mkOption {
      default = null;
      description = "Mutual-TLS worker ingress on the central host; required for peripheral placements.";
      type = types.nullOr (
        types.submodule {
          options = {
            listenAddress = mkOption {
              type = listen;
              description = "Literal host IP and port for authenticated worker traffic.";
            };
            serverCertificate = pathOption "Server PEM certificate or certificate chain.";
            serverKey = pathOption "Private server key in a root-only runtime file.";
            clientCaCertificate = pathOption "PEM trust anchors for replica certificates.";
            clientCertificatesDirectory = pathOption "Directory of <owner>-<worker>@<host>.pem replica certificate pins.";
          };
        }
      );
    };
    peripheral = mkOption {
      default = null;
      description = "Credentials and central worker endpoint for this peripheral.";
      type = types.nullOr (
        types.submodule {
          options = {
            address = endpointOption "Central worker ingress hostname or IP and port (without a URL scheme).";
            serverName = mkOption {
              type = types.nullOr types.nonEmptyStr;
              default = null;
              description = "Optional name against which to verify the server certificate.";
            };
            serverCaCertificate = pathOption "PEM trust anchors for the central worker ingress.";
            clientCredentialsDirectory = pathOption "Directory of <owner>-<worker>.pem and .key runtime files on this host.";
          };
        }
      );
    };
  };

  config = mkIf cfg.enable (mkMerge [
    {
      assertions = map (entry: {
        assertion =
          (entry.settings.placement.central || entry.settings.placement.peripherals != [ ])
          && lib.all validHost entry.settings.placement.peripherals
          && lib.unique entry.settings.placement.peripherals == entry.settings.placement.peripherals;
        message = "Worker ${entry.key} needs at least one placement and unique lowercase peripheral hostnames.";
      }) workers;
    }
    (mkIf (cfg.role == "central") {
      assertions = [
        {
          assertion = remoteWorkers == [ ] || ingress != null;
          message = "Peripheral worker placements require boreal.remote.central on the central host.";
        }
        {
          assertion = assigned == [ ];
          message = "The central host must not be listed under a worker's placement.peripherals.";
        }
        {
          assertion = client == null;
          message = "boreal.remote.peripheral is only for peripheral hosts.";
        }
      ];
    })
    (mkIf (cfg.role == "central" && remoteWorkers != [ ] && ingress != null) {
      environment.etc."boreal/remote-workers.json" = {
        source = ingressPolicy;
        user = "root";
        group = "root";
        mode = "0400";
      };
      systemd.services.boreal-broker = {
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        restartTriggers = [ ingressPolicy ];
        environment.BOREAL_REMOTE_WORKERS_CONFIG = "/etc/boreal/remote-workers.json";
      };
    })
    (mkIf (cfg.role == "peripheral") {
      assertions = [
        {
          assertion = assigned == [ ] || client != null;
          message = "Worker replicas on this peripheral require boreal.remote.peripheral.";
        }
        {
          assertion = ingress == null && !cfg.admin.enable && !cfg.updater.enable;
          message = "Boreal central ingress, restricted admin, and updater are not available on peripherals.";
        }
      ];

      users.groups = lib.genAttrs owners (_: { });
      users.users = lib.genAttrs owners (owner: {
        isSystemUser = true;
        group = owner;
        home = "/var/lib/boreal-peripheral/${owner}";
        createHome = true;
        homeMode = "0700";
      });
    })
    (mkIf (cfg.role == "peripheral" && client != null) {
      systemd.services = builtins.listToAttrs (
        map (
          entry:
          lib.nameValuePair "boreal-worker-${entry.key}" {
            description = "Boreal worker ${entry.key} (peripheral replica)";
            wantedBy = [ "multi-user.target" ];
            after = [ "network-online.target" ];
            wants = [ "network-online.target" ];
            environment = {
              TEMPORAL_ADDRESS = client.address;
              TEMPORAL_NAMESPACE = entry.owner;
              TEMPORAL_TLS = "true";
              TEMPORAL_TLS_SERVER_CA_CERT_PATH = "%d/server-ca.pem";
              TEMPORAL_TLS_CLIENT_CERT_PATH = "%d/client.pem";
              TEMPORAL_TLS_CLIENT_KEY_PATH = "%d/client.key";
            }
            // lib.optionalAttrs (client.serverName != null) {
              TEMPORAL_TLS_SERVER_NAME = client.serverName;
            };
            serviceConfig = {
              User = entry.owner;
              ExecStart = "${entry.settings.package}/bin/${entry.settings.package.outputName} ${entry.worker}";
              LoadCredential = [
                "server-ca.pem:${client.serverCaCertificate}"
                "client.pem:${client.clientCredentialsDirectory}/${entry.key}.pem"
                "client.key:${client.clientCredentialsDirectory}/${entry.key}.key"
              ];
              Restart = "on-failure";
              RestartSec = 5;
              TimeoutStopSec = "infinity";
              NoNewPrivileges = true;
              PrivateTmp = true;
              ProtectSystem = "strict";
              ProtectProc = "invisible";
            };
          }
        ) assigned
      );
    })
  ]);
}
