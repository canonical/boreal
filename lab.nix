{ self, pkgs, ... }:

let
  workers = self.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  boreal = {
    enable = true;

    role = "central";
    users.beijing = {
      keys = [ ];
      permissions = {
        read = false;
        write = false;
      };
      workers.beijing = {
        package = workers.lab-worker.beijing;
        placement.central = true;
      };
    };
  };
}
