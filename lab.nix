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
        package = workers.beijing-worker.beijing;
        placement.central = true;
      };
    };
  };
}
