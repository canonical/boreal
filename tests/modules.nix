{
  self,
  nixpkgs,
  pkgs,
}:

let
  inherit (nixpkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;

  trainBin = pkgs.writeShellScriptBin "train-worker" "exit 0";
  evalBin = pkgs.writeShellScriptBin "eval-worker" "exit 0";
  updaterStub = pkgs.writeShellScriptBin "boreal-updater" "exit 0";
  shellStub = pkgs.writeShellScriptBin "boreal-shell" "exit 0";
  brokerStub = pkgs.writeShellScriptBin "boreal-broker" "exit 0";
  adminStub = pkgs.runCommand "test-admin" { } ''
    mkdir -p "$out/bin"
    printf '#!/bin/sh\nexit 0\n' > "$out/bin/boreal-admin"
    printf '#!/bin/sh\nexit 0\n' > "$out/bin/boreal-admin-ctl"
    chmod +x "$out/bin/boreal-admin" "$out/bin/boreal-admin-ctl"
  '';

  baseContainer = {
    boot.isContainer = true;
    networking.hostName = "boreal-container";
    system.stateVersion = "26.05";
  };

  mkHost =
    extra:
    (self.lib.mkHost {
      inherit system;
      modules = [
        baseContainer
        extra
      ];
    }).config;

  sampleLabModule = {
    boreal = {
      enable = true;
      temporalAddress = "127.0.0.1:7234";
      shellPackage = shellStub;
      brokerPackage = brokerStub;
      admin.package = adminStub;
      workerPackages = {
        train-worker = {
          package = trainBin;
          workflowType = "TrainWorkflow";
          description = "Run a training job";
          args = [
            {
              name = "dataset";
              type = "string";
              description = "Dataset name";
            }
            {
              name = "mode";
              type = "enum";
              choices = [
                "fast"
                "accurate"
              ];
              default = "fast";
            }
            {
              name = "epochs";
              type = "int";
              min = 1;
              max = 500;
              default = 10;
            }
            {
              name = "learning_rate";
              type = "float";
              min = 0.0;
              max = 1.0;
              default = 0.05;
            }
            {
              name = "dry_run";
              type = "bool";
              default = false;
            }
          ];
        };
        eval-worker = {
          package = evalBin;
          workflowType = "EvalWorkflow";
          description = "Evaluate a checkpoint";
          args = [
            {
              name = "checkpoint";
              type = "string";
            }
          ];
        };
      };
      users = {
        alice = {
          keys = [ "ssh-ed25519 AAAA alice-test-key" ];
          permissions = {
            read = true;
            write = false;
          };
          workers = {
            train = {
              package = "train-worker";
              permissions.write = [
                "bob"
                "bob"
              ];
            };
            eval = {
              package = "eval-worker";
              permissions.read = [ "bob" ];
            };
          };
        };
        bob = {
          keys = [ "ssh-ed25519 BBBB bob-test-key" ];
          editMode = "helix";
          workers.eval.package = "eval-worker";
        };
      };
      updater = {
        enable = true;
        package = updaterStub;
      };
    };
  };

  configured = mkHost sampleLabModule;

  labCheckDrv = self.lib.mkLabCheck {
    inherit pkgs;
    modules = [ sampleLabModule ];
  };

  assertionFailed =
    config: text:
    builtins.any (entry: !entry.assertion && lib.hasInfix text entry.message) config.assertions;

  mkWorkerPkgHost =
    pkgSpec:
    mkHost {
      boreal = {
        enable = true;
        workerPackages.bad-worker = {
          package = trainBin;
          workflowType = "BadWorkflow";
        }
        // pkgSpec;
      };
    };

  unknownPackage = mkHost {
    boreal = {
      enable = true;
      users.alice.workers.train.package = "unregistered";
    };
  };
  emptyWorkflowType = mkWorkerPkgHost { workflowType = "   "; };
  invalidArgName = mkWorkerPkgHost {
    args = [
      {
        name = "bad-arg";
        type = "string";
      }
    ];
  };
  duplicateArgName = mkWorkerPkgHost {
    args = [
      {
        name = "x";
        type = "string";
      }
      {
        name = "x";
        type = "int";
      }
    ];
  };
  optionalBeforeRequired = mkWorkerPkgHost {
    args = [
      {
        name = "opt";
        type = "int";
        default = 1;
      }
      {
        name = "req";
        type = "string";
      }
    ];
  };
  missingEnumChoices = mkWorkerPkgHost {
    args = [
      {
        name = "kind";
        type = "enum";
      }
    ];
  };
  unexpectedChoices = mkWorkerPkgHost {
    args = [
      {
        name = "kind";
        type = "string";
        choices = [ "a" ];
      }
    ];
  };
  invertedBounds = mkWorkerPkgHost {
    args = [
      {
        name = "n";
        type = "int";
        min = 10;
        max = 5;
      }
    ];
  };
  boundsOnString = mkWorkerPkgHost {
    args = [
      {
        name = "s";
        type = "string";
        min = 1;
      }
    ];
  };
  floatBoundOnInt = mkWorkerPkgHost {
    args = [
      {
        name = "n";
        type = "int";
        min = 1.5;
      }
    ];
  };
  defaultTypeMismatch = mkWorkerPkgHost {
    args = [
      {
        name = "n";
        type = "int";
        default = "ten";
      }
    ];
  };
  defaultOutOfBounds = mkWorkerPkgHost {
    args = [
      {
        name = "n";
        type = "int";
        min = 1;
        max = 10;
        default = 20;
      }
    ];
  };
  defaultInvalidChoice = mkWorkerPkgHost {
    args = [
      {
        name = "kind";
        type = "enum";
        choices = [
          "a"
          "b"
        ];
        default = "c";
      }
    ];
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
      workerPackages.eval-worker = {
        package = evalBin;
        workflowType = "EvalWorkflow";
      };
      users.alice.workers.eval = {
        package = "eval-worker";
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
  adminCollisionUser = mkHost {
    boreal = {
      enable = true;
      users.admin = { };
    };
  };
  invalidWorkerName = mkHost {
    boreal = {
      enable = true;
      workerPackages.eval-worker = {
        package = evalBin;
        workflowType = "EvalWorkflow";
      };
      users.alice.workers."BadName".package = "eval-worker";
    };
  };
  collidingUnitKey = mkHost {
    boreal = {
      enable = true;
      workerPackages.eval-worker = {
        package = evalBin;
        workflowType = "EvalWorkflow";
      };
      users = {
        alice.workers."bob-eval".package = "eval-worker";
        alice-bob.workers.eval.package = "eval-worker";
      };
    };
  };
  emptyTemporalAddress = mkHost {
    boreal = {
      enable = true;
      temporalAddress = "   ";
    };
  };
  removedUpdaterRepo = mkHost {
    boreal = {
      enable = true;
      updater.repository = "https://github.com/example/lab.git";
    };
  };
  removedUpdaterBranch = mkHost {
    boreal = {
      enable = true;
      updater.branch = "main";
    };
  };
  removedUpdaterConfigName = mkHost {
    boreal = {
      enable = true;
      updater.configurationName = "lab-1";
    };
  };
  invalidUpdaterHost = mkHost {
    networking.hostName = lib.mkForce "bad_host";
    boreal = {
      enable = true;
      updater.enable = true;
    };
  };
  removedPocOption = mkHost { boreal.poc.enable = true; };

  vm =
    (self.lib.mkVm {
      system = "x86_64-linux";
      modules = [
        sampleLabModule
        {
          boreal.temporalAddress = lib.mkForce "127.0.0.1:7233";
          boreal.vm = {
            sshPort = 2223;
            devAuthorizedKeys = [ "ssh-ed25519 DDDD dev-key" ];
          };
        }
      ];
    }).config;

  workersEtc = configured.environment.etc."boreal/workers.yaml";
  alicePermsEtc = configured.environment.etc."boreal/users/alice/perms.yaml";
  bobPermsEtc = configured.environment.etc."boreal/users/bob/perms.yaml";
  updaterEtc = configured.environment.etc."boreal/updater.yaml";
  adminEtc = configured.environment.etc."boreal-admin/policy.yaml";

  expectedWorkersCatalog = pkgs.writeText "expected-workers.json" (
    builtins.toJSON {
      train-worker = {
        workflowType = "TrainWorkflow";
        description = "Run a training job";
        args = [
          {
            name = "dataset";
            type = "string";
            description = "Dataset name";
          }
          {
            name = "mode";
            type = "enum";
            description = "";
            choices = [
              "fast"
              "accurate"
            ];
            default = "fast";
          }
          {
            name = "epochs";
            type = "int";
            description = "";
            min = 1;
            max = 500;
            default = 10;
          }
          {
            name = "learning_rate";
            type = "float";
            description = "";
            min = 0.0;
            max = 1.0;
            default = 0.05;
          }
          {
            name = "dry_run";
            type = "bool";
            description = "";
            default = false;
          }
        ];
      };
      eval-worker = {
        workflowType = "EvalWorkflow";
        description = "Evaluate a checkpoint";
        args = [
          {
            name = "checkpoint";
            type = "string";
            description = "";
          }
        ];
      };
    }
  );

  expectedAlicePolicy = pkgs.writeText "expected-alice-perms.json" (
    builtins.toJSON {
      user = "alice";
      permissions = {
        read = true;
        write = false;
      };
      workers = {
        train = {
          package = "train-worker";
          permissions = {
            read = true;
            write = [ "bob" ];
          };
        };
        eval = {
          package = "eval-worker";
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
        eval = {
          package = "eval-worker";
          permissions = {
            read = false;
            write = false;
          };
        };
      };
    }
  );

  expectedUpdaterPolicy = pkgs.writeText "expected-updater.json" (
    builtins.toJSON {
      projectDir = "/etc/nixos";
      host = "boreal-container";
      allowSwitch = configured.system.switch.enable;
      brokerSocket = "/run/boreal/updater.sock";
      workerUnits = [
        "boreal-worker-alice-eval.service"
        "boreal-worker-alice-train.service"
        "boreal-worker-bob-eval.service"
      ];
    }
  );

  viHost = mkHost {
    boreal = {
      enable = true;
      users.alice.editMode = "vi";
    };
  };
  aliceRcSource = builtins.head configured.systemd.services.boreal-user-rc-alice.restartTriggers;
  bobRcSource = builtins.head configured.systemd.services.boreal-user-rc-bob.restartTriggers;
in
assert configured.boreal.users.alice.editMode == "emacs";
assert configured.boreal.users.bob.editMode == "helix";
assert viHost.boreal.users.alice.editMode == "vi";
assert configured.systemd.services.boreal-user-rc-alice.serviceConfig.User == "alice";
assert configured.systemd.services.boreal-user-rc-alice.serviceConfig.Type == "oneshot";
assert configured.systemd.services.boreal-user-rc-alice.serviceConfig.RemainAfterExit;
assert configured.systemd.services.boreal-user-rc-alice.serviceConfig.UMask == "0077";
assert builtins.elem "sshd.service" configured.systemd.services.boreal-user-rc-alice.before;
assert configured.systemd.services.boreal-user-rc-bob.serviceConfig.User == "bob";
assert configured.nix.settings.substituters == [ "https://cache.nixos.org/" ];
assert !configured.nix.gc.automatic;
assert builtins.hasAttr "boreal" configured.users.groups;
assert configured.users.users.alice.isNormalUser;
assert configured.users.users.alice.homeMode == "0700";
assert configured.users.users.alice.shell == "${shellStub}/bin/boreal-shell";
assert builtins.elem "boreal" configured.users.users.alice.extraGroups;
assert
  configured.users.users.alice.openssh.authorizedKeys.keys == [
    "ssh-ed25519 AAAA alice-test-key"
  ];
assert builtins.elem "boreal" configured.users.users.bob.extraGroups;
assert configured.users.users.bob.shell == "${shellStub}/bin/boreal-shell";
assert configured.users.users.admin.homeMode == "0700";
assert configured.users.users.admin.shell == "${adminStub}/bin/boreal-admin";
assert !(builtins.elem "wheel" configured.users.users.admin.extraGroups);
assert !(builtins.elem "boreal" configured.users.users.admin.extraGroups);
assert configured.security.sudo.wheelNeedsPassword;
assert workersEtc.user == "root" && workersEtc.group == "boreal" && workersEtc.mode == "0440";
assert
  alicePermsEtc.user == "root" && alicePermsEtc.group == "boreal" && alicePermsEtc.mode == "0440";
assert bobPermsEtc.user == "root" && bobPermsEtc.group == "boreal" && bobPermsEtc.mode == "0440";
assert updaterEtc.user == "root" && updaterEtc.group == "root" && updaterEtc.mode == "0400";
assert adminEtc.user == "root" && adminEtc.group == "boreal-admin" && adminEtc.mode == "0440";
assert builtins.all (rule: builtins.elem rule configured.systemd.tmpfiles.rules) [
  "d /etc/boreal 0750 root boreal -"
  "d /etc/boreal/users 0750 root boreal -"
  "d /etc/boreal/users/alice 0750 root boreal -"
  "d /etc/boreal/users/bob 0750 root boreal -"
  "d /etc/boreal-admin 0750 root boreal-admin -"
  "d /run/netns 0700 root root -"
];
assert !builtins.any (lib.hasInfix "/home/") configured.systemd.tmpfiles.rules;
assert configured.services.temporal.enable;
assert configured.services.postgresql.enable;
assert
  configured.systemd.services.postgresql.serviceConfig.NetworkNamespacePath
  == "/run/netns/boreal-internal";
assert
  configured.systemd.services.temporal.serviceConfig.NetworkNamespacePath
  == "/run/netns/boreal-internal";
assert configured.systemd.services.boreal-worker-alice-train.serviceConfig.User == "alice";
assert
  configured.systemd.services.boreal-worker-alice-train.serviceConfig.NetworkNamespacePath
  == "/run/netns/boreal-worker-alice-train";
assert
  configured.systemd.services.boreal-worker-alice-train.environment.TEMPORAL_ADDRESS
  == "127.0.0.1:7234";
assert
  configured.systemd.services.boreal-worker-alice-train.environment.TEMPORAL_NAMESPACE == "alice";
assert
  configured.systemd.services.boreal-worker-alice-train.environment.LAB_TASK_QUEUE == "alice-train";
assert
  configured.systemd.services.boreal-worker-alice-train.serviceConfig.ExecStart
  == "${trainBin}/bin/train-worker";
assert
  configured.systemd.services.boreal-worker-alice-eval.serviceConfig.ExecStart
  == "${evalBin}/bin/eval-worker";
assert configured.systemd.services.boreal-updater.serviceConfig.User == "root";
assert !configured.systemd.services.boreal-updater.restartIfChanged;
assert !configured.systemd.services.boreal-updater.stopIfChanged;
assert
  configured.systemd.services.boreal-updater.serviceConfig.ExecStart
  == "${updaterStub}/bin/boreal-updater run";
assert assertionFailed unknownPackage "boreal.workerPackages";
assert assertionFailed emptyWorkflowType "workflowType";
assert assertionFailed invalidArgName "argument names";
assert assertionFailed duplicateArgName "argument names";
assert assertionFailed optionalBeforeRequired "precede optional";
assert assertionFailed missingEnumChoices "choices";
assert assertionFailed unexpectedChoices "choices";
assert assertionFailed invertedBounds "min/max";
assert assertionFailed boundsOnString "min/max";
assert assertionFailed floatBoundOnInt "min/max";
assert assertionFailed defaultTypeMismatch "default values";
assert assertionFailed defaultOutOfBounds "default values";
assert assertionFailed defaultInvalidChoice "default values";
assert assertionFailed unknownDefaultPermUser "declared in boreal.users";
assert assertionFailed unknownWorkerPermUser "declared in boreal.users";
assert assertionFailed rootUser "must not be root";
assert assertionFailed adminCollisionUser "reserved";
assert assertionFailed invalidWorkerName "worker name must match";
assert assertionFailed collidingUnitKey "must be unique";
assert assertionFailed emptyTemporalAddress "temporalAddress";
assert assertionFailed removedUpdaterRepo "boreal.updater.repository";
assert assertionFailed removedUpdaterBranch "boreal.updater.branch";
assert assertionFailed removedUpdaterConfigName "boreal.updater.configurationName";
assert assertionFailed invalidUpdaterHost "networking.hostName";
assert assertionFailed removedPocOption "boreal.poc";
assert vm.boreal.enable;
assert vm.boreal.updater.enable;
assert vm.services.openssh.enable;
assert !vm.services.openssh.allowSFTP;
assert !vm.system.switch.enable;
assert vm.system.name == "boreal";
assert vm.networking.hostName == "boreal-vm";
assert vm.system.build.vm.meta.mainProgram == "run-boreal-vm";
assert vm.virtualisation.diskImage == "./boreal-vm.qcow2";
assert !vm.services.openssh.settings.PasswordAuthentication;
assert !vm.services.openssh.settings.KbdInteractiveAuthentication;
assert vm.services.openssh.settings.PermitRootLogin == "no";
assert vm.services.openssh.settings.AllowTcpForwarding == "no";
assert vm.services.openssh.settings.AllowStreamLocalForwarding == "no";
assert vm.services.openssh.settings.AllowAgentForwarding == "no";
assert vm.services.openssh.settings.PermitTunnel == "no";
assert builtins.elem "ssh-ed25519 DDDD dev-key" vm.users.users.alice.openssh.authorizedKeys.keys;
assert builtins.elem "ssh-ed25519 DDDD dev-key" vm.users.users.bob.openssh.authorizedKeys.keys;
assert vm.users.users.admin.openssh.authorizedKeys.keys == [ "ssh-ed25519 DDDD dev-key" ];
assert !(builtins.elem "wheel" vm.users.users.admin.extraGroups);
assert vm.security.sudo.wheelNeedsPassword;
assert vm.boreal.admin.allowPoweroff;
assert !(builtins.hasAttr "temporal-dev" vm.systemd.services);
assert vm.virtualisation.useNixStoreImage;
assert !vm.virtualisation.mountHostNixStore;
assert vm.virtualisation.sharedDirectories == { };
assert builtins.length vm.virtualisation.forwardPorts == 1;
assert builtins.all (
  port:
  port.from == "host"
  && port.proto == "tcp"
  && port.host.address == "127.0.0.1"
  && port.host.port == 2223
  && port.guest.port == 22
) vm.virtualisation.forwardPorts;
assert builtins.all
  (
    unitName:
    let
      service = vm.systemd.services.${unitName};
    in
    builtins.elem "boreal-broker.service" service.after
    && builtins.elem "boreal-temporal-namespaces.service" service.after
    && service.serviceConfig.RestartSec == 5
  )
  [
    "boreal-worker-alice-train"
    "boreal-worker-alice-eval"
    "boreal-worker-bob-eval"
  ];
pkgs.runCommand "boreal-module-tests"
  {
    inherit labCheckDrv;
    nativeBuildInputs = [
      pkgs.python3
      pkgs.remarshal
    ];
  }
  ''
    set -eu
    test -e "$labCheckDrv"
    if grep -Eq 'ssh-|/nix/store/' ${workersEtc.source} ${alicePermsEtc.source} ${bobPermsEtc.source} ${updaterEtc.source}; then
      echo "Key or store path leaked into generated policy YAML" >&2
      exit 1
    fi
    remarshal --if yaml --of json ${workersEtc.source} > workers.json
    remarshal --if yaml --of json ${alicePermsEtc.source} > alice.json
    remarshal --if yaml --of json ${bobPermsEtc.source} > bob.json
    remarshal --if yaml --of json ${updaterEtc.source} > updater.json
    remarshal --if yaml --of json ${aliceRcSource} > alice-rc.json
    remarshal --if yaml --of json ${bobRcSource} > bob-rc.json
    python3 - <<'PY'
    import json, pathlib

    for actual_path, expected_path in [
        ("workers.json", "${expectedWorkersCatalog}"),
        ("alice.json", "${expectedAlicePolicy}"),
        ("bob.json", "${expectedBobPolicy}"),
        ("updater.json", "${expectedUpdaterPolicy}"),
    ]:
        actual = json.loads(pathlib.Path(actual_path).read_text())
        expected = json.loads(pathlib.Path(expected_path).read_text())
        assert actual == expected, f"mismatch for {actual_path}: {actual!r} != {expected!r}"

    assert json.loads(pathlib.Path("alice-rc.json").read_text()) == {"edit_mode": "emacs"}
    assert json.loads(pathlib.Path("bob-rc.json").read_text()) == {"edit_mode": "helix"}
    PY
    touch "$out"
  ''
