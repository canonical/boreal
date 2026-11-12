{ nixpkgs, pkgs }:

let
  system = pkgs.stdenv.hostPlatform.system;
  mkHost = extra: (nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      ../modules
      ../hosts/poc.nix
      extra
    ];
  }).config;
  worker = pkgs.writeShellScriptBin "test-worker" "exit 0";
  configured = mkHost {
    boreal.poc = {
      enable = true;
      acknowledgeInsecureTemporal = true;
      users.alice.sshKeys = [ "ssh-ed25519 AAAA demo" ];
      workers.demo = {
        user = "alice";
        package = worker;
        environment.LAB_TASK_QUEUE = "alice-demo";
      };
      updater = {
        enable = true;
        package = worker;
      };
    };
  };
  missingAcknowledgement = mkHost { boreal.poc.enable = true; };
  unknownUser = mkHost {
    boreal.poc = {
      enable = true;
      acknowledgeInsecureTemporal = true;
      workers.demo = { user = "missing"; package = worker; };
    };
  };
  assertionFailed = config: text:
    builtins.any (entry: !entry.assertion && nixpkgs.lib.hasInfix text entry.message) config.assertions;
  vm = (nixpkgs.lib.nixosSystem {
    system = "x86_64-linux";
    modules = [
      ../modules
      ../hosts/boreal-vm.nix
    ];
  }).config;
  vmWorkers = [
    { name = "alice-greet"; user = "alice"; executable = "lab-worker"; }
    { name = "alice-count"; user = "alice"; executable = "lab-worker-alt"; }
    { name = "bob-greet"; user = "bob"; executable = "lab-worker"; }
    { name = "bob-count"; user = "bob"; executable = "lab-worker-alt"; }
  ];
in
assert configured.nix.settings.substituters == [
  "https://mirrors.bfsu.edu.cn/nix-channels/store"
  "https://mirror.sjtu.edu.cn/nix-channels/store"
  "https://cache.nixos.org/"
];
assert configured.users.users.alice.openssh.authorizedKeys.keys == [ "ssh-ed25519 AAAA demo" ];
assert configured.systemd.services.boreal-worker-demo.serviceConfig.User == "alice";
assert configured.systemd.services.boreal-worker-demo.environment.LAB_TASK_QUEUE == "alice-demo";
assert configured.systemd.services.boreal-updater.serviceConfig.User == "root";
assert configured.systemd.services.boreal-updater.serviceConfig.ExecStart == "${worker}/bin/test-worker run";
assert assertionFailed missingAcknowledgement "acknowledgeInsecureTemporal";
assert assertionFailed unknownUser "must belong to a user";
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
assert vm.users.users.alice.openssh.authorizedKeys.keys == vm.users.users.bob.openssh.authorizedKeys.keys;
assert vm.users.users.admin.openssh.authorizedKeys.keys == vm.users.users.alice.openssh.authorizedKeys.keys;
assert builtins.elem "wheel" vm.users.users.admin.extraGroups;
assert !vm.security.sudo.wheelNeedsPassword;
assert vm.virtualisation.useNixStoreImage;
assert !vm.virtualisation.mountHostNixStore;
assert vm.virtualisation.sharedDirectories == { };
assert builtins.length vm.virtualisation.forwardPorts == 1;
assert builtins.all (port:
  port.from == "host" && port.proto == "tcp" && port.host.address == "127.0.0.1"
  && port.host.port == 2222 && port.guest.port == 22
) vm.virtualisation.forwardPorts;
assert builtins.all ({ name, user, executable }:
  let service = vm.systemd.services."boreal-worker-${name}";
  in service.serviceConfig.User == user
    && service.environment.TEMPORAL_NAMESPACE == user
    && service.environment.LAB_TASK_QUEUE == name
    && nixpkgs.lib.hasSuffix "/bin/${executable}" service.serviceConfig.ExecStart
) vmWorkers;
assert vm.systemd.services.temporal-dev.serviceConfig.DynamicUser;
assert nixpkgs.lib.hasInfix "--ip 127.0.0.1" vm.systemd.services.temporal-dev.serviceConfig.ExecStart;
assert vm.boreal.poc.updater.enable;
pkgs.runCommand "boreal-module-tests" { } "touch $out"
