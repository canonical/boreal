{ self, config, lib, pkgs, ... }:

let
  workers = self.packages.${pkgs.stdenv.hostPlatform.system};
  authorizedKeys =
    users:
    let
      readKeyFile = file: lib.splitString "\n" (builtins.readFile file);
      perUser =
        user:
        let
          inherit (config.users.users.${user}.openssh.authorizedKeys) keys keyFiles;
        in
        keys ++ lib.concatMap readKeyFile keyFiles;
      lines = map lib.trim (lib.concatMap perUser users);
    in
    lib.unique (builtins.filter (line: line != "" && !lib.hasPrefix "#" line) lines);
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
        name = "gh:pseudocc";
        keyHash = "sha256-mkqqirN8HTatqcqfpYv4dth6YMko8N4IJ6Zm2nNHl7A=";
      };
    };
  };
}
