{ rust-overlay }:

let
  rustBin = pkgs: (pkgs.extend rust-overlay.overlays.default).rust-bin;
  toolchain =
    pkgs: (rustBin pkgs).stable.latest.default.override { extensions = [ "rust-analyzer" ]; };
  platform =
    pkgs: toolchain:
    pkgs.makeRustPlatform {
      cargo = toolchain;
      rustc = toolchain;
    };
in
{
  inherit rustBin toolchain platform;
}
