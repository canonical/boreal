{ self, pkgs, ... }:

let
  workers = self.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  boreal = {
    enable = true;

    workerPackages.example-worker = {
      package = workers.example-worker;
      workflowType = "ExampleWorkflow";
      description = "Run the starter example workflow";
      args = [
        {
          name = "message";
          type = "string";
          description = "Non-secret text message";
        }
      ];
    };

    users.alice = {
      keys = [
        # Add Alice's public SSH key here (never private keys):
        # "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA..."
      ];
      permissions = {
        read = false;
        write = false;
      };
      workers.example.package = "example-worker";
    };
  };
}
