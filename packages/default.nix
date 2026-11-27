{ pkgs, rustPlatform }:

{
  boreal-admin = pkgs.callPackage ./admin.nix { inherit rustPlatform; };
  boreal-broker = pkgs.callPackage ./broker.nix { inherit rustPlatform; };
  boreal-build = pkgs.callPackage ./build.nix { };
  boreal-shell = pkgs.callPackage ./shell.nix { inherit rustPlatform; };
  boreal-updater = pkgs.callPackage ./updater.nix { inherit rustPlatform; };
}
