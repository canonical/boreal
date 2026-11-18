{ self, pkgs, ... }:

let
  demoPackages = self.packages.${pkgs.stdenv.hostPlatform.system};
  # Public test SSH key shared by disposable demo accounts.
  publicKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINuoseDzo0mUwBthHFnKfNPK1EdJTrpv7boeC1ybMsty";
in
{
  boreal = {
    enable = true;

    users = {
      alice = {
        keys = [ publicKey ];
        permissions = {
          read = true;
          write = false;
        };
        workers = {
          greet = {
            package = demoPackages.lab-worker.lab-worker;
            permissions.write = [ "bob" ];
          };
          count = {
            package = demoPackages.lab-worker-alt.lab-worker-alt;
            permissions.read = [ "bob" ];
          };
        };
      };

      bob = {
        keys = [ publicKey ];
        workers = {
          greet.package = demoPackages.lab-worker.lab-worker;
          count.package = demoPackages.lab-worker-alt.lab-worker-alt;
        };
      };
    };
  };
}
