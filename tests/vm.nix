{
  self,
  pkgs,
}:

let
  greetWorker = self.lib.buildRustWorker pkgs {
    pname = "greet-worker";
    src = ./vm-worker;
    cargoLock.lockFile = ./vm-worker/Cargo.lock;
  };

  countWorker = self.lib.buildRustWorker pkgs {
    pname = "count-worker";
    src = ./vm-worker;
    cargoLock.lockFile = ./vm-worker/Cargo.lock;
  };

  timerArg = {
    name = "wait_seconds";
    type = "int";
    min = 1;
    max = 7200;
    default = 20;
    description = "Timer duration in seconds";
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
        workerPackages = {
          lab-worker = {
            package = greetWorker;
            workflowType = "DemoWorkflow";
            description = "Run durable greeting or child workflow";
            args = [
              {
                name = "name";
                type = "string";
                description = "Recipient name";
              }
              timerArg
            ];
          };
          lab-worker-alt = {
            package = countWorker;
            workflowType = "CountWorkflow";
            description = "Count Unicode scalar values";
            args = [
              {
                name = "name";
                type = "string";
                description = "Input text";
              }
              timerArg
            ];
          };
        };

        users = {
          alice = {
            permissions = {
              read = true;
              write = false;
            };
            workers = {
              greet = {
                package = "lab-worker";
                permissions.write = [ "bob" ];
              };
              count = {
                package = "lab-worker-alt";
                permissions.read = [ "bob" ];
              };
            };
          };
          bob = {
            workers = {
              greet.package = "lab-worker";
              count.package = "lab-worker-alt";
            };
          };
          carol = { };
        };

        updater = {
          enable = true;
          repository = "https://git.lab.internal:8443/demo-lab.git";
          branch = "downstreams/demo-lab";
          configurationName = "lab-vm";
        };
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
        "boreal-worker-alice-count.service",
        "boreal-worker-bob-greet.service",
        "boreal-worker-bob-count.service",
        "sshd.service",
    ]:
        lab.wait_for_unit(unit)

    print(lab.succeed("python3 ${./vm-suite.py} ${temporalCli}", timeout=300))
  '';
}
