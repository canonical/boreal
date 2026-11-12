{
  lib,
  modulesPath,
  pkgs,
  ...
}:

let
  packages = import ../packages { inherit pkgs; };
  # This is a public SSH key, intentionally shared by disposable lab accounts.
  publicKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINuoseDzo0mUwBthHFnKfNPK1EdJTrpv7boeC1ybMsty";
  mkWorker = user: workload: package: {
    inherit user package;
    environment = {
      TEMPORAL_ADDRESS = "127.0.0.1:7233";
      TEMPORAL_NAMESPACE = user;
      LAB_TASK_QUEUE = "${user}-${workload}";
    };
  };
  workerNames = [
    "alice-greet"
    "alice-count"
    "bob-greet"
    "bob-count"
  ];
in
{
  imports = [ "${modulesPath}/virtualisation/qemu-vm.nix" ];

  networking.hostName = "boreal-vm";
  networking.firewall.enable = true;
  system.name = "boreal";
  system.stateVersion = "26.05";
  system.switch.enable = false;

  # No password or SSH root login is provisioned for this disposable VM.
  users.users.root.initialHashedPassword = "!";
  users.users.admin = {
    isNormalUser = true;
    extraGroups = [ "wheel" ];
    openssh.authorizedKeys.keys = [ publicKey ];
  };
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
  environment.systemPackages = [
    packages.lab-client
    pkgs.temporal-cli
  ];

  boreal.poc = {
    enable = true;
    # Local users can reach all namespaces; this is not authorization.
    acknowledgeInsecureTemporal = true;
    users = {
      alice.sshKeys = [ publicKey ];
      bob.sshKeys = [ publicKey ];
    };
    workers = {
      alice-greet = mkWorker "alice" "greet" packages.lab-worker;
      alice-count = mkWorker "alice" "count" packages.lab-worker-alt;
      bob-greet = mkWorker "bob" "greet" packages.lab-worker;
      bob-count = mkWorker "bob" "count" packages.lab-worker-alt;
    };
    updater = {
      enable = true;
      package = packages.boreal-updater;
    };
  };

  # start-dev registers existing namespaces idempotently and keeps SQLite on
  # the persistent VM disk. All of its listeners bind to the specified IP.
  # The server may not yet be ready when systemd starts it; retry workers with
  # a bounded start rate rather than exhausting the default burst immediately.
  systemd.services =
    (lib.genAttrs (map (name: "boreal-worker-${name}") workerNames) (_: {
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
          ExecStart = "${lib.getExe pkgs.temporal-cli} server start-dev --headless --ip 127.0.0.1 --port 7233 --namespace alice --namespace bob --db-filename /var/lib/temporal-dev/temporal.sqlite";
          Restart = "on-failure";
          RestartSec = 5;
        };
      };
    };

  virtualisation = {
    # Preserve the existing default disk path when the system name changes.
    diskImage = "./boreal-vm.qcow2";
    memorySize = 6144;
    cores = 4;
    diskSize = 16384;
    graphics = false;
    # Keep the VM independent of host store and shared directories. The writable
    # overlay and Temporal state persist in the VM's qcow2 root disk.
    useNixStoreImage = true;
    writableStore = true;
    writableStoreUseTmpfs = false;
    sharedDirectories = lib.mkForce { };
    forwardPorts = [
      {
        from = "host";
        host.address = "127.0.0.1";
        host.port = 2222;
        guest.port = 22;
      }
    ];
  };
}
