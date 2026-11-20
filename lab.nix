{ self, pkgs, ... }:

let
  demo = self.packages.${pkgs.stdenv.hostPlatform.system};
  vaultPackage = demo.demo-vault.demo-vault;
in
{
  boreal = {
    enable = true;

    users.alice = {
      editMode = "vi";
      workers.desk.package = demo.demo-desk.demo-desk;
      vault = {
        package = vaultPackage;
        grants.desk = [ "check-desk-badge" ];
      };
    };

    users.shared = {
      workers.dispatch = {
        package = demo.demo-dispatch.demo-dispatch;
        permissions = {
          read = [ "alice" ];
          write = [ "alice" ];
        };
      };
      vault = {
        package = vaultPackage;
        grants.dispatch = [ "check-dispatch-key" ];
      };
    };
  };
}
