{ ... }:

{
  # Evaluation and build target only; this does not describe physical hardware.
  boot.isContainer = true;
  networking.hostName = "boreal-container";
  system.stateVersion = "26.05";
}
