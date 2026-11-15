{ pkgs }:

{
  boreal-admin = pkgs.callPackage ./admin.nix { };
  boreal-broker = pkgs.callPackage ./broker.nix { };
  boreal-shell = pkgs.callPackage ./shell.nix { };
  boreal-updater = pkgs.callPackage ./updater.nix { };
}
