{
  self,
  pkgs,
  publicKey,
  ...
}:

let
  demoPackages = self.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  boreal = {
    enable = true;

    users = {
      alice = {
        keys = [ publicKey ];
        vault = {
          package = demoPackages.lab-vault.lab-vault;
          grants.greet = [ "verify-demo-token" ];
        };
        permissions = {
          read = true;
          write = false;
        };
        workers = {
          greet = {
            package = demoPackages.lab-worker.lab-worker;
            permissions.write = [ "bob" ];
            placement = {
              central = true;
              peripherals = [ "compute-01" ];
            };
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
