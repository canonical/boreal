{ pkgs }:

{
  boreal-shell = pkgs.callPackage ./shell.nix { };
  boreal-updater = pkgs.callPackage ./updater.nix { };
}
