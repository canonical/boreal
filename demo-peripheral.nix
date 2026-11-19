{ demo, ... }:

{
  boot.isNspawnContainer = true;
  networking = {
    hostName = "compute-01";
    useDHCP = false;
    enableIPv6 = false;
  };
  system.stateVersion = "26.05";

  boreal = {
    role = "peripheral";
    remote.peripheral = {
      address = "${demo.centralIp}:${toString demo.port}";
      serverCaCertificate = "${demo.clientDirectory}/ca.pem";
      clientCredentialsDirectory = demo.clientDirectory;
    };
  };
}
