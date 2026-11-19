{
  self,
  pkgs,
}:

let
  labWorker = self.lib.buildRustWorker pkgs {
    pname = "lab-worker";
    src = ./vm-worker;
    cargoLock.lockFile = ./vm-worker/Cargo.lock;
    outputs = [
      "out"
      "greet"
      "greet-schedule"
      "count"
      "schema"
    ];
  };

  temporalCli = "${pkgs.temporal-cli}/bin/temporal";
in
pkgs.testers.runNixOSTest {
  name = "boreal-integration";

  nodes.lab =
    { lib, pkgs, ... }:
    {
      imports = [ self.nixosModules.default ];

      virtualisation = {
        cores = 4;
        memorySize = 4096;
        diskSize = 12288;
        writableStore = true;
      };

      boot.loader.grub.enable = false;
      system.switch.enable = true;
      networking.hostName = "lab-vm";
      networking.extraHosts = "127.0.0.1 git.lab.internal";
      system.stateVersion = "26.05";
      _module.args.self.rev = "d9c49750000000000000000000000000000000000";

      nix.settings = {
        experimental-features = [
          "nix-command"
          "flakes"
        ];
        sandbox = false;
      };

      services.openssh = {
        enable = true;
        openFirewall = true;
        settings.AuthenticationMethods = "publickey";
        authorizedKeysFiles = lib.mkForce [ "/run/test-auth-keys/%u" ];
      };

      environment.systemPackages = with pkgs; [
        git
        iproute2
        openssl
        openssh
        python3
        util-linux
      ];

      boreal = {
        enable = true;
        deploymentRelease = {
          rev = "d9c49750000000000000000000000000000000000";
          tags = [ "v1.2.3" ];
        };
        users = {
          alice = {
            permissions = {
              read = true;
              write = false;
            };
            workers = {
              greet = {
                package = labWorker.greet;
                schedule = {
                  enable = true;
                  package = labWorker.greet-schedule;
                };
                permissions.write = [ "bob" ];
              };
              count = {
                package = labWorker.count;
                permissions.read = [ "bob" ];
              };
            };
          };
          bob = {
            workers = {
              greet = {
                package = labWorker.greet;
                schedule.enable = false;
              };
              count.package = labWorker.count;
            };
          };
          carol = { };
        };

        updater.enable = true;
      };

      specialisation.v2.configuration = {
        environment.etc."boreal-updated-generation".text = "v2-activated\n";
      };
    };

  testScript = ''
    lab.start()
    for unit in [
        "postgresql.service",
        "boreal-temporal-db.service",
        "temporal.service",
        "boreal-temporal-namespaces.service",
        "boreal-broker.service",
        "boreal-worker-alice-greet.service",
        "boreal-schedule-alice-greet.service",
        "boreal-worker-alice-count.service",
        "boreal-worker-bob-greet.service",
        "boreal-worker-bob-count.service",
        "sshd.service",
    ]:
        lab.wait_for_unit(unit)

    print(lab.succeed("python3 ${./vm-suite.py} ${temporalCli}", timeout=300))
  '';
}
