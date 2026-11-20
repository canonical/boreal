{
  self,
  nixpkgs,
  pkgs,
}:

let
  inherit (nixpkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;

  mkWorker =
    {
      name,
      workflow,
      description ? "",
    }:
    pkgs.runCommand name
      {
        outputs = [
          "out"
          name
          "schema"
        ];
      }
      ''
                dest="$(printenv ${name})"
                schema_out="$(printenv schema)"
                mkdir -p "$out" "$dest/bin" "$schema_out"
                printf '#!/bin/sh\nexit 0\n' > "$dest/bin/${name}"
                chmod +x "$dest/bin/${name}"
                cat > "$schema_out/schema.yaml" <<EOF
        ${name}:
          workflow: ${workflow}
          description: "${description}"
          args: []
        EOF
      '';
  trainBin = mkWorker {
    name = "train";
    workflow = "TrainWorkflow";
    description = "Run a training job";
  };
  evalBin = mkWorker {
    name = "eval";
    workflow = "EvalWorkflow";
    description = "Evaluate a checkpoint";
  };
  helloBin =
    pkgs.runCommand "hello-worker"
      {
        outputs = [
          "out"
          "hello"
          "hello-schedule"
          "schema"
        ];
      }
      ''
        hello="$(printenv hello)"
        schedule="$(printenv hello-schedule)"
        schema_out="$(printenv schema)"
        mkdir -p "$out" "$hello/bin" "$schedule/bin" "$schema_out"
        printf '#!/bin/sh\nexit 0\n' > "$hello/bin/hello"
        printf '#!/bin/sh\nexit 0\n' > "$schedule/bin/hello-schedule"
        chmod +x "$hello/bin/hello" "$schedule/bin/hello-schedule"
        printf 'hello:\n  workflow: HelloWorkflow\n  args: []\n' > "$schema_out/schema.yaml"
      '';
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
      users = {
        alice = {
          keys = [ "ssh-ed25519 AAAA alice-test-key" ];
          permissions = {
            read = true;
            write = false;
          };
          workers = {
            train = {
              package = trainBin.train;
              permissions.write = [
                "bob"
                "bob"
              ];
            };
            eval = {
              package = evalBin.eval;
              permissions.read = [ "bob" ];
            };
          };
        };
        bob = {
          keys = [ "ssh-ed25519 BBBB bob-test-key" ];
          editMode = "helix";
          workers.eval.package = evalBin.eval;
        };
      };
      updater = {
        enable = true;
        package = updaterStub;
      };
    };
  };

  configured = mkHost sampleLabModule;
  vaultBin = self.lib.buildRustVault pkgs {
    pname = "lab-vault";
    src = ./vm-worker;
    cargoLock.lockFile = ./vm-worker/Cargo.lock;
  };
  vaulted = mkHost {
    imports = [ sampleLabModule ];
    boreal.users.alice.vault = {
      package = vaultBin.lab-vault;
      grants.train = [ "verify-fixture-token" ];
    };
  };
  invalidVault = mkHost {
    imports = [ sampleLabModule ];
    boreal.users.alice.vault = {
      package = trainBin.train;
      grants.train = [ "verify-fixture-token" ];
    };
  };

  remoteLab = {
    boreal = {
      enable = true;
      users.alice.workers = {
        train = {
          package = trainBin.train;
          placement.peripherals = [ "compute-01" ];
        };
        daily = {
          package = helloBin.hello;
          placement = {
            central = false;
            peripherals = [ "compute-01" ];
          };
          schedule = {
            enable = true;
            package = helloBin.hello-schedule;
          };
        };
      };
    };
  };
  remoteHost =
    name: extra:
    self.lib.mkHost {
      inherit system;
      modules = [
        (baseContainer // { networking.hostName = name; })
        remoteLab
        extra
      ];
    };
  central = remoteHost "lab" {
    boreal.remote.central = {
      listenAddress = "192.168.42.1:7443";
      serverCertificate = "/run/pki/server.pem";
      serverKey = "/run/pki/server.key";
      clientCaCertificate = "/run/pki/ca.pem";
      clientCertificatesDirectory = "/run/pki/pins";
    };
    boreal.updater.package = updaterStub;
    boreal.updater.enable = true;
  };
  peripheral = remoteHost "compute-01" {
    boreal.role = "peripheral";
    boreal.remote.peripheral = {
      address = "192.168.42.1:7443";
      serverCaCertificate = "/run/pki/ca.pem";
      clientCredentialsDirectory = "/run/pki/client";
    };
  };
  fleetCheck = self.lib.mkFleetCheck {
    inherit pkgs;
    hosts = {
      lab = central;
      compute-01 = peripheral;
    };
  };
  centralServices = central.config.systemd.services;
  peripheralServices = peripheral.config.systemd.services;
  remoteWorker = peripheralServices.boreal-worker-alice-train;
  remotePolicy = central.config.environment.etc."boreal/remote-workers.json";
  remotePins =
    map (replica: replica.certificate)
      (builtins.fromJSON (builtins.readFile remotePolicy.source)).replicas;

  labCheckDrv = self.lib.mkLabCheck {
    inherit pkgs;
    modules = [ sampleLabModule ];
  };

  assertionFailed =
    config: text:
    builtins.any (entry: !entry.assertion && lib.hasInfix text entry.message) config.assertions;

  missingSchema = mkHost {
    boreal = {
      enable = true;
      users.alice.workers.train.package = pkgs.writeShellScriptBin "train" "exit 0";
    };
  };
  missingSchedulePackage = mkHost {
    boreal = {
      enable = true;
      users.alice.workers.train = {
        package = trainBin.train;
        schedule.enable = true;
      };
    };
  };
  otherTrain = mkWorker {
    name = "train";
    workflow = "OtherWorkflow";
  };
  duplicateOutputName = mkHost {
    boreal = {
      enable = true;
      users.alice.workers = {
        one.package = trainBin.train;
        two.package = otherTrain.train;
      };
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
      users.alice.workers.eval = {
        package = evalBin.eval;
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
      users.alice.workers."BadName".package = evalBin.eval;
    };
  };
  collidingUnitKey = mkHost {
    boreal = {
      enable = true;
      users = {
        alice.workers."bob-eval".package = evalBin.eval;
        alice-bob.workers.eval.package = evalBin.eval;
      };
    };
  };
  emptyTemporalAddress = mkHost {
    boreal = {
      enable = true;
      temporalAddress = "   ";
    };
  };
  invalidLabVersion = mkHost {
    boreal = {
      enable = true;
      version = "0.1.0 beta";
    };
  };
  workerVersionPkg =
    pkgs.runCommand "versioned-worker"
      {
        version = "0.4.2";
        outputs = [
          "out"
          "versioned"
          "schema"
        ];
      }
      ''
        dest="$(printenv versioned)"
        schema_out="$(printenv schema)"
        mkdir -p "$out" "$dest/bin" "$schema_out"
        printf '#!/bin/sh\nexit 0\n' > "$dest/bin/versioned"
        chmod +x "$dest/bin/versioned"
        printf 'versioned:\n  workflow: VersionedWorkflow\n  args: []\n' > "$schema_out/schema.yaml"
      '';
  workerVersionHost = mkHost {
    boreal = {
      enable = true;
      users.alice.workers.v.package = workerVersionPkg.versioned;
    };
  };
  mockDownstreamRoot = pkgs.runCommand "mock-downstream-root" { } ''
    mkdir -p "$out/temporal"
    printf '[workspace]\nresolver = "3"\n[workspace.package]\nversion = "1.2.3"\n' > "$out/temporal/Cargo.toml"
  '';
  selfManifestHost =
    (self.lib.mkHost {
      inherit system;
      specialArgs = {
        self = {
          outPath = mockDownstreamRoot;
          sourceInfo.rev = "d9c49750000000000000000000000000000000000";
        };
      };
      modules = [
        baseContainer
        { boreal.enable = true; }
      ];
    }).config;
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

  versionEtc = configured.environment.etc."boreal/version";
  deploymentEtc = selfManifestHost.environment.etc."boreal-deployment";
  schemasEtc = configured.environment.etc."boreal/schemas.yaml";
  alicePermsEtc = configured.environment.etc."boreal/users/alice/perms.yaml";
  bobPermsEtc = configured.environment.etc."boreal/users/bob/perms.yaml";
  updaterEtc = configured.environment.etc."boreal/updater.yaml";
  adminEtc = configured.environment.etc."boreal-admin/policy.yaml";

  expectedSchemas = pkgs.writeText "expected-schemas.json" (
    builtins.toJSON {
      directories = [
        evalBin.schema
        trainBin.schema
      ];
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
          package = "train";
          permissions = {
            read = true;
            write = [ "bob" ];
          };
        };
        eval = {
          package = "eval";
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
          package = "eval";
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
  schedulerHost = mkHost {
    boreal = {
      enable = true;
      temporalAddress = "127.0.0.1:7234";
      shellPackage = shellStub;
      brokerPackage = brokerStub;
      admin.package = adminStub;
      updater = {
        enable = true;
        package = updaterStub;
      };
      users.alice.workers.daily = {
        package = helloBin.hello;
        schedule = {
          enable = true;
          package = helloBin.hello-schedule;
        };
      };
    };
  };
  scheduleService = schedulerHost.systemd.services.boreal-schedule-alice-daily;
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
assert configured.boreal.version == "0.1.0";
assert workerVersionHost.boreal.version == "0.4.2";
assert selfManifestHost.boreal.version == "1.2.3";
assert selfManifestHost.environment.etc."boreal/version".text == "1.2.3\n";
assert versionEtc.user == "root" && versionEtc.group == "boreal" && versionEtc.mode == "0440";
assert versionEtc.text == "0.1.0\n";
assert import ./deployment-label.nix;
assert !assertionFailed vaulted "Vault for alice";
assert assertionFailed invalidVault "Vault for alice";
assert vaulted.environment.etc."boreal/vaults.json".mode == "0400";
assert vaulted.systemd.services.boreal-worker-alice-vault.serviceConfig.User == "alice";
assert vaulted.systemd.services.boreal-worker-alice-train.serviceConfig.User != "alice";
assert vaulted.systemd.services.boreal-worker-alice-train.serviceConfig.DynamicUser;
assert vaulted.systemd.services.boreal-worker-alice-train.serviceConfig.ProtectHome;
assert vaulted.systemd.services.boreal-worker-bob-eval.serviceConfig.User == "bob";
assert builtins.elem "boreal-netns-worker-alice-vault.service"
  vaulted.systemd.services.boreal-broker.requires;
assert import ./fleet.nix { inherit lib; };
assert fleetCheck.drvPath != "";
assert !configured.environment.etc ? "boreal/remote-workers.json";
assert centralServices ? boreal-worker-alice-train;
assert !centralServices ? boreal-worker-alice-daily;
assert centralServices ? boreal-schedule-alice-daily;
assert remotePolicy.user == "root" && remotePolicy.mode == "0400";
assert
  remotePins == [
    "/run/pki/pins/alice-daily@compute-01.pem"
    "/run/pki/pins/alice-train@compute-01.pem"
  ];
assert
  centralServices.boreal-broker.environment.BOREAL_REMOTE_WORKERS_CONFIG
  == "/etc/boreal/remote-workers.json";
assert !peripheral.config.services.temporal.enable && !peripheral.config.services.postgresql.enable;
assert !peripheralServices ? boreal-broker && !peripheralServices ? boreal-schedule-alice-daily;
assert !peripheral.config.users.users ? admin && peripheral.config.users.users.alice.isSystemUser;
assert peripheralServices ? boreal-worker-alice-daily;
assert !remoteWorker.serviceConfig ? NetworkNamespacePath;
assert remoteWorker.environment.TEMPORAL_ADDRESS == "192.168.42.1:7443";
assert
  remoteWorker.environment.TEMPORAL_NAMESPACE == "alice"
  && remoteWorker.environment.TEMPORAL_TLS == "true";
assert builtins.elem "client.key:/run/pki/client/alice-train.key"
  remoteWorker.serviceConfig.LoadCredential;
assert deploymentEtc.text == "d9c4975\n";
assert
  deploymentEtc.user == "root" && deploymentEtc.group == "root" && deploymentEtc.mode == "0444";
assert configured.systemd.services.boreal-user-rc-admin.serviceConfig.User == "admin";
assert schemasEtc.user == "root" && schemasEtc.group == "boreal" && schemasEtc.mode == "0440";
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
assert !(configured.systemd.services.boreal-worker-alice-train.environment ? LAB_TASK_QUEUE);
assert
  configured.systemd.services.boreal-worker-alice-train.serviceConfig.ExecStart
  == "${trainBin.train}/bin/train train";
assert
  configured.systemd.services.boreal-worker-alice-eval.serviceConfig.ExecStart
  == "${evalBin.eval}/bin/eval eval";
assert !configured.systemd.services ? boreal-schedule-alice-train;
assert scheduleService.serviceConfig.Type == "oneshot";
assert scheduleService.serviceConfig.RemainAfterExit;
assert scheduleService.serviceConfig.User == "alice";
assert scheduleService.serviceConfig.NetworkNamespacePath == "/run/netns/boreal-worker-alice-daily";
assert
  schedulerHost.systemd.services.boreal-worker-alice-daily.serviceConfig.ExecStart
  == "${helloBin.hello}/bin/hello daily";
assert
  scheduleService.serviceConfig.ExecStart == "${helloBin.hello-schedule}/bin/hello-schedule daily";
assert scheduleService.environment.TEMPORAL_NAMESPACE == "alice";
assert !(scheduleService.environment ? LAB_TASK_QUEUE);
assert !(scheduleService.serviceConfig ? PrivateNetwork);
assert lib.hasInfix "boreal-schedule-alice-daily.service" (
  builtins.readFile schedulerHost.environment.etc."boreal/updater.yaml".source
);
assert configured.systemd.services.boreal-updater.serviceConfig.User == "root";
assert !configured.systemd.services.boreal-updater.restartIfChanged;
assert !configured.systemd.services.boreal-updater.stopIfChanged;
assert
  configured.systemd.services.boreal-updater.serviceConfig.ExecStart
  == "${updaterStub}/bin/boreal-updater run";
assert assertionFailed missingSchema "schema output";
assert assertionFailed missingSchedulePackage "schedule.package";
assert assertionFailed duplicateOutputName "output names must be unique";
assert assertionFailed unknownDefaultPermUser "declared in boreal.users";
assert assertionFailed unknownWorkerPermUser "declared in boreal.users";
assert assertionFailed rootUser "must not be root";
assert assertionFailed adminCollisionUser "reserved";
assert assertionFailed invalidWorkerName "worker name must match";
assert assertionFailed collidingUnitKey "must be unique";
assert assertionFailed emptyTemporalAddress "temporalAddress";
assert assertionFailed invalidLabVersion "boreal.version";
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
    if grep -Eq 'ssh-' ${schemasEtc.source} ${alicePermsEtc.source} ${bobPermsEtc.source} ${updaterEtc.source}; then
      echo "Key leaked into generated policy YAML" >&2
      exit 1
    fi
    if grep -Eq '/nix/store/' ${alicePermsEtc.source} ${bobPermsEtc.source} ${updaterEtc.source}; then
      echo "Store path leaked into generated policy YAML" >&2
      exit 1
    fi
    remarshal --if yaml --of json ${schemasEtc.source} > schemas.json
    remarshal --if yaml --of json ${alicePermsEtc.source} > alice.json
    remarshal --if yaml --of json ${bobPermsEtc.source} > bob.json
    remarshal --if yaml --of json ${updaterEtc.source} > updater.json
    remarshal --if yaml --of json ${aliceRcSource} > alice-rc.json
    remarshal --if yaml --of json ${bobRcSource} > bob-rc.json
    python3 - <<'PY'
    import json, pathlib

    for actual_path, expected_path in [
        ("schemas.json", "${expectedSchemas}"),
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
