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
in
assert configured.nix.settings.substituters == [
  "https://mirrors.bfsu.edu.cn/nix-channels/store"
  "https://mirror.sjtu.edu.cn/nix-channels/store"
  "https://cache.nixos.org/"
];
assert configured.users.users.alice.openssh.authorizedKeys.keys == [ "ssh-ed25519 AAAA demo" ];
assert configured.systemd.services.boreal-worker-demo.serviceConfig.User == "alice";
assert configured.systemd.services.boreal-worker-demo.environment.LAB_TASK_QUEUE == "alice-demo";
assert assertionFailed missingAcknowledgement "acknowledgeInsecureTemporal";
assert assertionFailed unknownUser "must belong to a user";
pkgs.runCommand "boreal-module-tests" { } "touch $out"
