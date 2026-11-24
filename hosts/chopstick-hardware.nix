{
  config,
  lib,
  modulesPath,
  ...
}:

{
  imports = [ (modulesPath + "/installer/scan/not-detected.nix") ];

  boot.initrd = {
    availableKernelModules = [
      "xhci_pci"
      "ahci"
      "nvme"
      "usbhid"
      "usb_storage"
      "sd_mod"
    ];
    systemd = {
      enable = true;
      tpm2.enable = true;
    };
    luks.devices.chopstick-crypt = {
      device = "/dev/disk/by-label/chopstick-crypt";
      crypttabExtraOpts = [ "tpm2-device=auto" ];
    };
  };

  boot.kernelModules = [ "kvm-intel" ];
  boot.kernelParams = [ "nohibernate" ];
  hardware.cpu.intel.updateMicrocode = lib.mkDefault config.hardware.enableRedistributableFirmware;

  fileSystems = {
    "/" = {
      label = "chopstick-root";
      fsType = "ext4";
    };
    "/boot" = {
      label = "CHOP-EFI";
      fsType = "vfat";
      options = [ "umask=0077" ];
    };
    "/var/lib/lab-cache" = {
      label = "chopstick-cache";
      fsType = "ext4";
    };
  };

  swapDevices = [ { label = "chopstick-swap"; } ];
}
