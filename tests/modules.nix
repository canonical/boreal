{ nixpkgs, pkgs }:

let
  inherit (nixpkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;
  mkHost =
    extra:
    (lib.nixosSystem {
      inherit system;
      modules = [
        ../modules
        ../hosts/container.nix
        extra
      ];
    }).config;
  updaterStub = pkgs.writeShellScriptBin "test-updater" "exit 0";

  configured = mkHost {
    boreal = {
      enable = true;
      temporalAddress = "127.0.0.1:7234";
      users = {
        alice = {
          keys = [ "ssh-ed25519 AAAA alice-test-key" ];
          permissions = {
            read = true;
            write = false;
          };
          workers = {
            greet = {
              package = "lab-worker";
              permissions.write = [
                "bob"
                "bob"
              ];
            };
            count = {
              package = "lab-worker-alt";
              permissions.read = [ "bob" ];
            };
          };
        };
        bob = {
          keys = [ "ssh-ed25519 BBBB bob-test-key" ];
          workers.greet.package = "lab-worker";
        };
      };
      updater = {
        enable = true;
        package = updaterStub;
      };
    };
  };

  assertionFailed =
    config: text:
    builtins.any (entry: !entry.assertion && lib.hasInfix text entry.message) config.assertions;

  unknownPackage = mkHost {
    boreal = {
      enable = true;
      users.alice.workers.greet.package = "boreal-shell";
    };
  };
  unknownDefaultPermUser = mkHost {
    boreal = {
      enable = true;
      users.alice.permissions.write = [ "mallory" ];
    };
  };
  unknownWorkerPermUser = mkHost {
    boreal = {
      enable = true;
      users.alice.workers.greet = {
        package = "lab-worker";
        permissions.read = [ "mallory" ];
      };
    };
  };
  rootUser = mkHost {
    boreal = {
      enable = true;
      users.root = { };
    };
  };
  invalidWorkerName = mkHost {
    boreal = {
      enable = true;
      users.alice.workers."BadName".package = "lab-worker";
    };
  };
  collidingUnitKey = mkHost {
    boreal = {
      enable = true;
      users = {
        alice.workers."bob-greet".package = "lab-worker";
        alice-bob.workers.greet.package = "lab-worker";
      };
    };
  };
  emptyTemporalAddress = mkHost {
    boreal = {
      enable = true;
      temporalAddress = "   ";
    };
  };
  removedPocOption = mkHost { boreal.poc.enable = true; };

  vm =
    (lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ../modules
        ../hosts/boreal-vm.nix
      ];
    }).config;

  vmWorkers = [
    {
      name = "alice-greet";
      user = "alice";
      executable = "lab-worker";
    }
    {
      name = "alice-count";
      user = "alice";
      executable = "lab-worker-alt";
    }
    {
      name = "bob-greet";
      user = "bob";
      executable = "lab-worker";
    }
    {
      name = "bob-count";
      user = "bob";
      executable = "lab-worker-alt";
    }
  ];

  alicePermsEtc = configured.environment.etc."boreal/users/alice/perms.yaml";
  bobPermsEtc = configured.environment.etc."boreal/users/bob/perms.yaml";

  expectedAlicePolicy = pkgs.writeText "expected-alice-perms.json" (
    builtins.toJSON {
      user = "alice";
      permissions = {
        read = true;
        write = false;
      };
      workers = {
        greet = {
          package = "lab-worker";
          permissions = {
            read = true;
            write = [ "bob" ];
          };
        };
        count = {
          package = "lab-worker-alt";
          permissions = {
            read = [ "bob" ];
            write = false;
          };
        };
      };
    }
  );

  expectedBobPolicy = pkgs.writeText "expected-bob-perms.json" (
    builtins.toJSON {
      user = "bob";
      permissions = {
        read = false;
        write = false;
      };
      workers = {
        greet = {
          package = "lab-worker";
          permissions = {
            read = false;
            write = false;
          };
        };
      };
    }
  );
in
assert
  configured.nix.settings.substituters == [
    "https://mirrors.bfsu.edu.cn/nix-channels/store"
    "https://mirror.sjtu.edu.cn/nix-channels/store"
    "https://cache.nixos.org/"
  ];
assert builtins.hasAttr "boreal" configured.users.groups;
assert configured.users.users.alice.isNormalUser;
assert builtins.elem "boreal" configured.users.users.alice.extraGroups;
assert
  configured.users.users.alice.openssh.authorizedKeys.keys == [
    "ssh-ed25519 AAAA alice-test-key"
  ];
assert builtins.elem "boreal" configured.users.users.bob.extraGroups;
assert alicePermsEtc.user == "root";
assert alicePermsEtc.group == "boreal";
assert alicePermsEtc.mode == "0440";
assert bobPermsEtc.user == "root";
assert bobPermsEtc.group == "boreal";
assert bobPermsEtc.mode == "0440";
assert builtins.all (rule: builtins.elem rule configured.systemd.tmpfiles.rules) [
  "d /etc/boreal 0750 root boreal -"
  "d /etc/boreal/users 0750 root boreal -"
  "d /etc/boreal/users/alice 0750 root boreal -"
  "d /etc/boreal/users/bob 0750 root boreal -"
];
assert !builtins.any (lib.hasInfix "/home/") configured.systemd.tmpfiles.rules;
assert configured.systemd.services.boreal-worker-alice-greet.serviceConfig.User == "alice";
assert
  configured.systemd.services.boreal-worker-alice-greet.environment.TEMPORAL_ADDRESS
  == "127.0.0.1:7234";
assert
  configured.systemd.services.boreal-worker-alice-greet.environment.TEMPORAL_NAMESPACE == "alice";
assert
  configured.systemd.services.boreal-worker-alice-greet.environment.LAB_TASK_QUEUE == "alice-greet";
assert lib.hasSuffix "/bin/lab-worker"
  configured.systemd.services.boreal-worker-alice-greet.serviceConfig.ExecStart;
assert lib.hasSuffix "/bin/lab-worker-alt"
  configured.systemd.services.boreal-worker-alice-count.serviceConfig.ExecStart;
assert
  configured.systemd.services.boreal-worker-alice-count.environment.LAB_TASK_QUEUE == "alice-count";
assert configured.systemd.services.boreal-updater.serviceConfig.User == "root";
assert
  configured.systemd.services.boreal-updater.serviceConfig.ExecStart
  == "${updaterStub}/bin/test-updater run";
assert assertionFailed unknownPackage "known Boreal worker packages";
assert assertionFailed unknownDefaultPermUser "declared in boreal.users";
assert assertionFailed unknownWorkerPermUser "declared in boreal.users";
assert assertionFailed rootUser "must not be root";
assert assertionFailed invalidWorkerName "worker name must match";
assert assertionFailed collidingUnitKey "must be unique";
assert assertionFailed emptyTemporalAddress "temporalAddress";
assert assertionFailed removedPocOption "boreal.poc";
assert vm.boreal.enable;
assert vm.boreal.updater.enable;
assert vm.environment.etc."boreal/users/alice/perms.yaml".mode == "0440";
assert vm.environment.etc."boreal/users/bob/perms.yaml".mode == "0440";
assert vm.services.openssh.enable;
assert !vm.system.switch.enable;
assert vm.system.name == "boreal";
assert vm.networking.hostName == "boreal-vm";
assert vm.system.build.vm.meta.mainProgram == "run-boreal-vm";
assert vm.virtualisation.diskImage == "./boreal-vm.qcow2";
assert !vm.services.openssh.settings.PasswordAuthentication;
assert !vm.services.openssh.settings.KbdInteractiveAuthentication;
assert vm.services.openssh.settings.PermitRootLogin == "no";
assert vm.users.users.alice.openssh.authorizedKeys.keys != [ ];
assert
  vm.users.users.alice.openssh.authorizedKeys.keys == vm.users.users.bob.openssh.authorizedKeys.keys;
assert
  vm.users.users.admin.openssh.authorizedKeys.keys
  == vm.users.users.alice.openssh.authorizedKeys.keys;
assert builtins.elem "wheel" vm.users.users.admin.extraGroups;
assert !vm.security.sudo.wheelNeedsPassword;
assert vm.virtualisation.useNixStoreImage;
assert !vm.virtualisation.mountHostNixStore;
assert vm.virtualisation.sharedDirectories == { };
assert builtins.length vm.virtualisation.forwardPorts == 1;
assert builtins.all (
  port:
  port.from == "host"
  && port.proto == "tcp"
  && port.host.address == "127.0.0.1"
  && port.host.port == 2222
  && port.guest.port == 22
) vm.virtualisation.forwardPorts;
assert builtins.all (
  {
    name,
    user,
    executable,
  }:
  let
    service = vm.systemd.services."boreal-worker-${name}";
  in
  service.serviceConfig.User == user
  && service.environment.TEMPORAL_ADDRESS == "127.0.0.1:7233"
  && service.environment.TEMPORAL_NAMESPACE == user
  && service.environment.LAB_TASK_QUEUE == name
  && lib.hasSuffix "/bin/${executable}" service.serviceConfig.ExecStart
) vmWorkers;
assert vm.systemd.services.temporal-dev.serviceConfig.DynamicUser;
assert lib.hasInfix "--ip 127.0.0.1" vm.systemd.services.temporal-dev.serviceConfig.ExecStart;
pkgs.runCommand "boreal-module-tests"
  {
    nativeBuildInputs = [
      pkgs.python3
      pkgs.remarshal
    ];
  }
  ''
    set -eu
    if grep -q 'ssh-ed25519' ${alicePermsEtc.source} ${bobPermsEtc.source}; then
      echo "SSH key leaked into perms.yaml" >&2
      exit 1
    fi
    remarshal --if yaml --of json ${alicePermsEtc.source} > alice.json
    remarshal --if yaml --of json ${bobPermsEtc.source} > bob.json
    python3 - <<'PY'
    import json, pathlib

    for actual_path, expected_path in [
        ("alice.json", "${expectedAlicePolicy}"),
        ("bob.json", "${expectedBobPolicy}"),
    ]:
        actual = json.loads(pathlib.Path(actual_path).read_text())
        expected = json.loads(pathlib.Path(expected_path).read_text())
        assert actual == expected, f"mismatch for {actual_path}: {actual!r} != {expected!r}"
    PY
    touch "$out"
  ''
