{
  config,
  lib,
  modulesPath,
  pkgs,
  ...
}:

let
  vmCfg = config.boreal.vm;
  inherit (lib) mkOption types;

  userNames = builtins.attrNames config.boreal.users;
  namespaces = if userNames == [ ] then [ "default" ] else userNames;
  namespaceFlags = lib.concatMapStringsSep " " (
    ns: "--namespace ${lib.escapeShellArg ns}"
  ) namespaces;

  workerUnits = lib.concatLists (
    lib.mapAttrsToList (
      user: userCfg: map (worker: "boreal-worker-${user}-${worker}") (builtins.attrNames userCfg.workers)
    ) config.boreal.users
  );
in
{
  imports = [ "${modulesPath}/virtualisation/qemu-vm.nix" ];

  options.boreal.vm = {
    hostName = mkOption {
      type = types.str;
      default = "boreal-vm";
      description = "Hostname assigned to the disposable QEMU guest.";
    };

    systemName = mkOption {
      type = types.str;
      default = "boreal";
      description = "System name determining the run-<name>-vm launcher executable.";
    };

    diskImage = mkOption {
      type = types.str;
      default = "./boreal-vm.qcow2";
      description = "Default qcow2 image path when NIX_DISK_IMAGE is unset.";
    };

    sshPort = mkOption {
      type = types.port;
      default = 2222;
      description = "Host loopback TCP port forwarded to guest SSH port 22.";
    };

    memorySize = mkOption {
      type = types.ints.positive;
      default = 6144;
      description = "Guest RAM allocation in MiB.";
    };

    cores = mkOption {
      type = types.ints.positive;
      default = 4;
      description = "Guest virtual CPU count.";
    };

    diskSize = mkOption {
      type = types.ints.positive;
      default = 16384;
      description = "Guest root virtual disk size in MiB.";
    };

    adminKeys = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Public SSH keys authorized for the disposable VM admin account.";
    };

    devAuthorizedKeys = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Developer public SSH keys appended to admin and all boreal.users in the VM.";
    };
  };

  config = {
    networking.hostName = lib.mkDefault vmCfg.hostName;
    networking.firewall.enable = lib.mkDefault true;
    system.name = lib.mkDefault vmCfg.systemName;
    system.stateVersion = lib.mkDefault "26.05";
    system.switch.enable = false;

    users.users = {
      root.initialHashedPassword = "!";
      admin = {
        isNormalUser = true;
        extraGroups = [ "wheel" ];
        openssh.authorizedKeys.keys = lib.unique (vmCfg.adminKeys ++ vmCfg.devAuthorizedKeys);
      };
    }
    // lib.mapAttrs (_: _: {
      openssh.authorizedKeys.keys = vmCfg.devAuthorizedKeys;
    }) config.boreal.users;

    security.sudo = {
      enable = true;
      wheelNeedsPassword = false;
    };

    services.openssh = {
      enable = true;
      openFirewall = true;
      settings = {
        PasswordAuthentication = false;
        KbdInteractiveAuthentication = false;
        AuthenticationMethods = "publickey";
        PermitRootLogin = "no";
        AllowTcpForwarding = "no";
        X11Forwarding = false;
      };
    };

    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    environment.systemPackages = [ pkgs.temporal-cli ];

    systemd.services =
      (lib.genAttrs workerUnits (_: {
        after = [ "temporal-dev.service" ];
        wants = [ "temporal-dev.service" ];
        unitConfig = {
          StartLimitIntervalSec = 300;
          StartLimitBurst = 12;
        };
        serviceConfig.RestartSec = 5;
      }))
      // {
        temporal-dev = {
          description = "Local-only Temporal development server";
          wantedBy = [ "multi-user.target" ];
          unitConfig = {
            StartLimitIntervalSec = 300;
            StartLimitBurst = 6;
          };
          serviceConfig = {
            DynamicUser = true;
            StateDirectory = "temporal-dev";
            StateDirectoryMode = "0700";
            ExecStart = "${lib.getExe pkgs.temporal-cli} server start-dev --headless --ip 127.0.0.1 --port 7233 ${namespaceFlags} --db-filename /var/lib/temporal-dev/temporal.sqlite";
            Restart = "on-failure";
            RestartSec = 5;
          };
        };
      };

    virtualisation = {
      diskImage = vmCfg.diskImage;
      memorySize = vmCfg.memorySize;
      cores = vmCfg.cores;
      diskSize = vmCfg.diskSize;
      graphics = false;
      useNixStoreImage = true;
      writableStore = true;
      writableStoreUseTmpfs = false;
      sharedDirectories = lib.mkForce { };
      forwardPorts = [
        {
          from = "host";
          host.address = "127.0.0.1";
          host.port = vmCfg.sshPort;
          guest.port = 22;
        }
      ];
    };
  };
}
