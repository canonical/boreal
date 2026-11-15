{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.boreal;
  userNames = builtins.attrNames cfg.users;
  namespaces = lib.unique ([ "default" ] ++ userNames);

  workerEntries = lib.concatLists (
    lib.mapAttrsToList (
      user: userCfg:
      lib.mapAttrsToList (worker: _: {
        inherit user worker;
        unitKey = "${user}-${worker}";
      }) userCfg.workers
    ) cfg.users
  );

  mkNetnsService = nsName: description: {
    inherit description;
    wantedBy = [ "multi-user.target" ];
    before = [ "network.target" ];
    restartIfChanged = false;
    stopIfChanged = false;
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = pkgs.writeShellScript "setup-netns-${nsName}" ''
        set -eu
        mkdir -p -m 0700 /run/netns
        chmod 0700 /run/netns
        if [ ! -e /run/netns/${lib.escapeShellArg nsName} ]; then
          ${pkgs.iproute2}/bin/ip netns add ${lib.escapeShellArg nsName}
        fi
        ${pkgs.iproute2}/bin/ip -n ${lib.escapeShellArg nsName} link set lo up
      '';
      ExecStop = pkgs.writeShellScript "cleanup-netns-${nsName}" ''
        ${pkgs.iproute2}/bin/ip netns delete ${lib.escapeShellArg nsName} 2>/dev/null || true
      '';
    };
  };

  workerNetnsServices = builtins.listToAttrs (
    map (entry: {
      name = "boreal-netns-worker-${entry.unitKey}";
      value = mkNetnsService "boreal-worker-${entry.unitKey}" "Isolated network namespace for Boreal worker ${entry.unitKey}";
    }) workerEntries
  );

  workerNetnsUnits = map (entry: "boreal-netns-worker-${entry.unitKey}.service") workerEntries;
in
{
  config = lib.mkIf cfg.enable {
    users.groups = {
      temporal = { };
      boreal-pg = { };
      boreal-broker = { };
    };

    users.users = {
      temporal = {
        isSystemUser = true;
        group = "temporal";
        extraGroups = [ "boreal-pg" ];
        home = "/var/lib/temporal";
      };
      boreal-broker = {
        isSystemUser = true;
        group = "boreal-broker";
        extraGroups = [
          "boreal"
          "boreal-pg"
        ];
        home = "/var/lib/boreal-broker";
      };
      postgres.extraGroups = [ "boreal-pg" ];
    };

    services.postgresql = {
      enable = true;
      ensureDatabases = [
        "temporal"
        "temporal_visibility"
      ];
      ensureUsers = [
        {
          name = "temporal";
          ensureDBOwnership = true;
        }
        {
          name = "boreal-broker";
        }
      ];
      settings = {
        listen_addresses = lib.mkForce "127.0.0.1";
        unix_socket_directories = lib.mkForce "/run/postgresql";
        unix_socket_group = lib.mkForce "boreal-pg";
        unix_socket_permissions = lib.mkForce "0770";
      };
      authentication = lib.mkForce ''
        local all postgres peer
        local temporal,temporal_visibility temporal peer
        local temporal boreal-broker peer
        host temporal,temporal_visibility temporal 127.0.0.1/32 trust
      '';
    };

    services.temporal = {
      enable = true;
      user = "temporal";
      group = "temporal";
      settings = {
        log = {
          stdout = true;
          level = "warn";
        };
        global.membership = {
          maxJoinDuration = "30s";
          broadcastAddress = "127.0.0.1";
        };
        services = {
          frontend.rpc = {
            grpcPort = 7233;
            membershipPort = 6933;
            bindOnLocalHost = true;
            httpPort = 7243;
          };
          matching.rpc = {
            grpcPort = 7235;
            membershipPort = 6935;
            bindOnLocalHost = true;
          };
          history.rpc = {
            grpcPort = 7234;
            membershipPort = 6934;
            bindOnLocalHost = true;
          };
          worker.rpc = {
            grpcPort = 7239;
            membershipPort = 6939;
            bindOnLocalHost = true;
          };
        };
        persistence = {
          defaultStore = "postgres-default";
          visibilityStore = "postgres-visibility";
          numHistoryShards = 4;
          datastores = {
            postgres-default.sql = {
              pluginName = "postgres12";
              databaseName = "temporal";
              connectAddr = "127.0.0.1:5432";
              connectProtocol = "tcp";
              user = "temporal";
              password = "";
              maxConns = 20;
              maxIdleConns = 20;
              maxConnLifetime = "1h";
            };
            postgres-visibility.sql = {
              pluginName = "postgres12";
              databaseName = "temporal_visibility";
              connectAddr = "127.0.0.1:5432";
              connectProtocol = "tcp";
              user = "temporal";
              password = "";
              maxConns = 4;
              maxIdleConns = 4;
              maxConnLifetime = "1h";
            };
          };
        };
        clusterMetadata = {
          enableGlobalNamespace = false;
          failoverVersionIncrement = 10;
          masterClusterName = "active";
          currentClusterName = "active";
          clusterInformation.active = {
            enabled = true;
            initialFailoverVersion = 1;
            rpcName = "frontend";
            rpcAddress = "127.0.0.1:7233";
            httpAddress = "127.0.0.1:7243";
          };
        };
        dcRedirectionPolicy.policy = "noop";
        archival = {
          history.state = "disabled";
          visibility.state = "disabled";
        };
        namespaceDefaults.archival = {
          history.state = "disabled";
          visibility.state = "disabled";
        };
      };
    };

    systemd.services = workerNetnsServices // {
      boreal-netns-internal = mkNetnsService "boreal-internal" "Isolated network namespace for Boreal Temporal and PostgreSQL";

      postgresql = {
        after = [ "boreal-netns-internal.service" ];
        requires = [ "boreal-netns-internal.service" ];
        serviceConfig = {
          NetworkNamespacePath = "/run/netns/boreal-internal";
          SupplementaryGroups = [ "boreal-pg" ];
          SystemCallFilter = [ "@chown" ];
        };
      };

      postgresql-setup = {
        after = [ "boreal-netns-internal.service" ];
        requires = [ "boreal-netns-internal.service" ];
        serviceConfig.NetworkNamespacePath = "/run/netns/boreal-internal";
      };

      boreal-temporal-db = {
        description = "Initialize and migrate Boreal Temporal PostgreSQL schemas";
        wantedBy = [ "multi-user.target" ];
        after = [
          "boreal-netns-internal.service"
          "postgresql.service"
          "postgresql-setup.service"
        ];
        requires = [
          "boreal-netns-internal.service"
          "postgresql.service"
          "postgresql-setup.service"
        ];
        before = [ "temporal.service" ];
        path = [
          config.services.postgresql.package
          config.services.temporal.package
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          User = "postgres";
          Group = "postgres";
          NetworkNamespacePath = "/run/netns/boreal-internal";
          PrivateTmp = true;
          ProtectHome = true;
          ProtectSystem = "strict";
          NoNewPrivileges = true;
        };
        script = ''
          set -eu
          psql -v ON_ERROR_STOP=1 -d postgres <<'SQL'
          ALTER DATABASE temporal OWNER TO temporal;
          ALTER DATABASE temporal_visibility OWNER TO temporal;
          GRANT ALL ON SCHEMA public TO temporal;
          SQL
          psql -v ON_ERROR_STOP=1 -d temporal -c "GRANT ALL ON SCHEMA public TO temporal;"
          psql -v ON_ERROR_STOP=1 -d temporal_visibility -c "GRANT ALL ON SCHEMA public TO temporal;"

          has_main=$(psql -tAc "SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'schema_version'" -d temporal)
          if [ "$has_main" != "1" ]; then
            temporal-sql-tool --plugin postgres12 --ep 127.0.0.1 -p 5432 -u temporal --db temporal setup-schema -v 0.0
          fi
          temporal-sql-tool --plugin postgres12 --ep 127.0.0.1 -p 5432 -u temporal --db temporal update-schema -d ${config.services.temporal.package}/share/schema/postgresql/v12/temporal/versioned

          has_vis=$(psql -tAc "SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'schema_version'" -d temporal_visibility)
          if [ "$has_vis" != "1" ]; then
            temporal-sql-tool --plugin postgres12 --ep 127.0.0.1 -p 5432 -u temporal --db temporal_visibility setup-schema -v 0.0
          fi
          temporal-sql-tool --plugin postgres12 --ep 127.0.0.1 -p 5432 -u temporal --db temporal_visibility update-schema -d ${config.services.temporal.package}/share/schema/postgresql/v12/visibility/versioned

          psql -v ON_ERROR_STOP=1 -d temporal <<'SQL'
          GRANT CONNECT ON DATABASE temporal TO "boreal-broker";
          GRANT USAGE ON SCHEMA public TO "boreal-broker";
          GRANT SELECT ON TABLE namespaces, executions, current_executions TO "boreal-broker";
          SQL
        '';
      };

      temporal = {
        after = [
          "boreal-netns-internal.service"
          "boreal-temporal-db.service"
        ];
        requires = [
          "boreal-netns-internal.service"
          "boreal-temporal-db.service"
        ];
        serviceConfig = {
          DynamicUser = lib.mkForce false;
          NetworkNamespacePath = "/run/netns/boreal-internal";
        };
      };

      boreal-temporal-namespaces = {
        description = "Register Boreal user namespaces in Temporal";
        wantedBy = [ "multi-user.target" ];
        after = [
          "boreal-netns-internal.service"
          "temporal.service"
        ];
        requires = [
          "boreal-netns-internal.service"
          "temporal.service"
        ];
        path = [ pkgs.temporal-cli ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          User = "temporal";
          Group = "temporal";
          NetworkNamespacePath = "/run/netns/boreal-internal";
          PrivateTmp = true;
          ProtectHome = true;
          ProtectSystem = "strict";
          NoNewPrivileges = true;
        };
        script = ''
          set -eu
          for _ in $(seq 1 90); do
            if temporal operator cluster health --address 127.0.0.1:7233 >/dev/null 2>&1; then
              break
            fi
            sleep 1
          done
          temporal operator cluster health --address 127.0.0.1:7233 >/dev/null
          for ns in ${lib.escapeShellArgs namespaces}; do
            if ! temporal operator namespace describe --address 127.0.0.1:7233 --namespace "$ns" >/dev/null 2>&1; then
              temporal operator namespace create --address 127.0.0.1:7233 --namespace "$ns"
            fi
          done
        '';
      };

      boreal-broker = {
        description = "Boreal Temporal policy broker, worker gate, and drain coordinator";
        wantedBy = [ "multi-user.target" ];
        after = [
          "boreal-netns-internal.service"
          "boreal-temporal-namespaces.service"
        ]
        ++ workerNetnsUnits;
        requires = [
          "boreal-netns-internal.service"
          "boreal-temporal-namespaces.service"
        ]
        ++ workerNetnsUnits;
        environment = {
          BOREAL_ETC_DIR = "/etc/boreal";
          BOREAL_BROKER_STATE_DIR = "/var/lib/boreal-broker";
          BOREAL_BROKER_SOCKET = "/run/boreal/broker.sock";
          BOREAL_UPDATER_SOCKET = "/run/boreal/updater.sock";
          BOREAL_INTERNAL_NETNS = "/run/netns/boreal-internal";
          BOREAL_WORKER_NETNS_DIR = "/run/netns";
          BOREAL_WORKER_LISTEN_ADDR = cfg.temporalAddress;
          BOREAL_UPSTREAM_TEMPORAL_ADDR = "127.0.0.1:7233";
          BOREAL_PG_SOCKET = "/run/postgresql/.s.PGSQL.5432";
          BOREAL_PG_USER = "boreal-broker";
          BOREAL_PG_DB = "temporal";
          BOREAL_BROKER_USER = "boreal-broker";
          BOREAL_SOCKET_GROUP = "boreal";
          BOREAL_PG_GROUP = "boreal-pg";
        };
        serviceConfig = {
          User = "root";
          Group = "boreal";
          RuntimeDirectory = "boreal";
          RuntimeDirectoryMode = "0750";
          StateDirectory = "boreal-broker";
          StateDirectoryMode = "0700";
          ExecStart = lib.getExe cfg.brokerPackage;
          Restart = "always";
          RestartSec = 2;
          PrivateTmp = true;
          ProtectHome = true;
        };
      };
    };
  };
}
