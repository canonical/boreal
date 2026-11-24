{ self, config, lib, pkgs, ... }:

let
  workers = self.packages.${pkgs.stdenv.hostPlatform.system};
  authorizedKeys = let
    perUser = user: config.users.users.${user}.openssh.authorizedKeys.keys;
  in
    users: lib.uniq (lib.concatMap perUser users);
in
{
  boreal = {
    enable = true;

    admin.keys = authorizedKeys [
      "pseudoc"
    ];

    users.sutton = let
      members = [
        "pseudoc"
      ];
    in {
      keys = authorizedKeys members;

      permissions = {
        read = members;
        write = members;
      };

      workers.beijing = {
        package = workers.lab-worker.beijing;
        placement.central = true;
      };
    };

    users.pseudoc = {
      accounts.github = {
        name = "pseudocc";
        keyHash = "sha256-mkqqirN8HTatqcqfpYv4dth6YMko8N4IJ6Zm2nNHl7A=";
      };
    };
  };
}
