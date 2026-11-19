{ pkgs }:

{
  boreal-admin = pkgs.callPackage ./admin.nix { };
  boreal-broker = pkgs.callPackage ./broker.nix { };
  boreal-build = pkgs.callPackage ./build.nix { };
  boreal-shell = pkgs.callPackage ./shell.nix { };
  boreal-updater = pkgs.callPackage ./updater.nix { };
}
