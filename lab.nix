{ self, pkgs, ... }:

let
  demoPackages = self.packages.${pkgs.stdenv.hostPlatform.system};
  # Public test SSH key shared by disposable demo accounts.
  publicKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINuoseDzo0mUwBthHFnKfNPK1EdJTrpv7boeC1ybMsty";
  timerArg = {
    name = "wait_seconds";
    type = "int";
    min = 1;
    max = 7200;
    default = 20;
    description = "Durable timer duration in seconds";
  };
in
{
  boreal = {
    enable = true;

    workerPackages = {
      lab-worker = {
        package = demoPackages.lab-worker;
        workflowType = "DemoWorkflow";
        description = "Schedule a durable timer and greet the named recipient";
        args = [
          {
            name = "name";
            type = "string";
            description = "Recipient name";
          }
          timerArg
        ];
      };

      lab-worker-alt = {
        package = demoPackages.lab-worker-alt;
        workflowType = "CountWorkflow";
        description = "Schedule a durable timer and count Unicode scalar values";
        args = [
          {
            name = "name";
            type = "string";
            description = "Input text to count";
          }
          timerArg
        ];
      };
    };

    users = {
      alice = {
        keys = [ publicKey ];
        permissions = {
          read = true;
          write = false;
        };
        workers = {
          greet = {
            package = "lab-worker";
            permissions.write = [ "bob" ];
          };
          count = {
            package = "lab-worker-alt";
            permissions.read = [ "bob" ];
          };
        };
      };

      bob = {
        keys = [ publicKey ];
        workers = {
          greet.package = "lab-worker";
          count.package = "lab-worker-alt";
        };
      };
    };
  };
}
