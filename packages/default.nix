{ pkgs }:

let
  mkTemporalPackage = crate: pkgs.callPackage ./temporal.nix { inherit crate; };
in
{
  lab-worker = mkTemporalPackage "lab-worker";
  lab-worker-alt = mkTemporalPackage "lab-worker-alt";
  lab-client = mkTemporalPackage "lab-client";
  boreal-shell = pkgs.callPackage ./shell.nix { };
  boreal-updater = pkgs.callPackage ./updater.nix { };
}
