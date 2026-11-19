{
  config,
  lib,
  modulesPath,
  ...
}:

let
  vmCfg = config.boreal.vm;
  inherit (lib) mkOption types;

  workerUnits = lib.concatLists (
    lib.mapAttrsToList (
      user: userCfg:
      lib.concatLists (
        lib.mapAttrsToList (
          worker: workerCfg:
          lib.optional (
            if config.boreal.role == "central" then
              workerCfg.placement.central
            else
              builtins.elem config.networking.hostName workerCfg.placement.peripherals
          ) "boreal-worker-${user}-${worker}"
        ) userCfg.workers
      )
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
    system.switch.enable = lib.mkDefault false;

    boreal.admin = lib.mkIf (config.boreal.role == "central") {
      enable = lib.mkDefault true;
      allowPoweroff = lib.mkDefault true;
      keys = lib.unique (vmCfg.adminKeys ++ vmCfg.devAuthorizedKeys);
    };

    users.users = lib.mkIf (config.boreal.role == "central") (
      lib.mapAttrs (_: _: {
        openssh.authorizedKeys.keys = vmCfg.devAuthorizedKeys;
      }) config.boreal.users
    );

    services.openssh = {
      enable = true;
      openFirewall = true;
      settings.AuthenticationMethods = "publickey";
    };

    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];

    systemd.services = lib.genAttrs workerUnits (_: {
      unitConfig = {
        StartLimitIntervalSec = 300;
        StartLimitBurst = 12;
      };
      serviceConfig.RestartSec = 5;
    });

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
