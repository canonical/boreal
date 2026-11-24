{ pkgs, ... }:

{
  imports = [ ./chopstick-hardware.nix ];

  boot = {
    loader = {
      efi.canTouchEfiVariables = true;
      systemd-boot.editor = false;
    };
    lanzaboote = {
      enable = true;
      pkiBundle = "/var/lib/sbctl";
      configurationLimit = 4;
      measuredBoot = {
        enable = true;
        pcrs = [
          0
          4
          7
        ];
      };
    };
  };
  environment.systemPackages = [ pkgs.sbctl ];

  networking.hostName = "chopstick";
  networking.useDHCP = true;
  time.timeZone = "Asia/Shanghai";

  services.openssh = {
    enable = true;
    openFirewall = false;
  };
  networking.firewall.interfaces.eno2.allowedTCPPorts = [ 22 ];

  boreal = {
    role = "central";
    updater.enable = true;
  };
  system.stateVersion = "26.05";
}
