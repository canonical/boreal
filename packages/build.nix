{
  git,
  lib,
  nix,
  writeShellApplication,
}:

writeShellApplication {
  name = "boreal-build";
  runtimeInputs = [
    git
    nix
  ];
  text = ''
    if [[ $# -lt 2 || $# -gt 3 || ! ''${3:-system} =~ ^(system|vm)$ ]]; then
      echo "usage: boreal-build CHECKOUT NIXOS_CONFIGURATION [system|vm]" >&2
      exit 2
    fi
    checkout=$(cd -- "$1" && pwd -P)
    if [[ ! $checkout =~ ^/[a-zA-Z0-9._/-]+$ ]]; then
      echo "boreal-build: checkout path contains characters unsupported in flake URLs" >&2
      exit 2
    fi
    root=$(git -C "$checkout" rev-parse --show-toplevel)
    status=$(git -C "$checkout" status --porcelain --untracked-files=no)
    if [[ $root != "$checkout" || -n $status ]]; then
      echo "boreal-build: use the root of a clean downstream Git checkout" >&2
      exit 2
    fi
    rev=$(git -C "$checkout" rev-parse --verify 'HEAD^{commit}')
    tags=$(git -C "$checkout" tag --points-at "$rev")
    exec nix --extra-experimental-features 'nix-command flakes' build \
      --no-update-lock-file --no-write-lock-file --option pure-eval true \
      --expr ${lib.escapeShellArg (builtins.readFile ../updater/build-release.nix)} \
      --argstr ref "git+file://$checkout?rev=$rev" --argstr rev "$rev" \
      --argstr host "$2" --argstr tags "$tags" "''${3:-system}"
  '';
}
