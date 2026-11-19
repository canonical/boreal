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
    usage() {
      echo "usage: boreal-build CHECKOUT NIXOS_CONFIGURATION [system|vm] [--override-input boreal FLAKE]" >&2
      exit 2
    }
    if [[ $# -lt 2 ]]; then
      usage
    fi
    checkout=$(cd -- "$1" && pwd -P)
    host=$2
    shift 2
    target=toplevel
    case ''${1:-} in
      system) shift ;;
      vm) target=vm; shift ;;
    esac
    override=()
    if [[ $# -ne 0 ]]; then
      [[ $# -eq 3 && $1 == --override-input && $2 == boreal ]] || usage
      override=("$@")
    fi
    if [[ ! $checkout =~ ^/[a-zA-Z0-9._/-]+$ ]]; then
      echo "boreal-build: checkout path contains characters unsupported in flake URLs" >&2
      exit 2
    fi
    if [[ ! $host =~ ^[a-zA-Z0-9_-]+$ ]]; then
      echo "boreal-build: invalid NixOS configuration name" >&2
      exit 2
    fi
    root=$(git -C "$checkout" rev-parse --show-toplevel)
    if [[ $root != "$checkout" ]]; then
      echo "boreal-build: use the root of a downstream Git checkout" >&2
      exit 2
    fi
    rev=$(git -C "$checkout" rev-parse --verify 'HEAD^{commit}')
    [[ $rev =~ ^[0-9a-f]{40}$ ]] || { echo "boreal-build: unsupported Git revision" >&2; exit 2; }
    status=$(git -C "$checkout" status --porcelain --untracked-files=no)
    ref="git+file://$checkout"
    dirty=false
    tags=""
    if [[ -z $status ]]; then
      ref="$ref?rev=$rev"
      exact_tags=$(git -C "$checkout" tag --points-at "$rev")
      while IFS= read -r tag; do
        # Only interpolate inert tag strings. Nix selects version tags.
        if [[ $tag =~ ^[a-zA-Z0-9.+-]{1,128}$ ]]; then
          tags+="\"$tag\" "
        fi
      done <<< "$exact_tags"
    else
      dirty=true
    fi

    # Evaluate the flake installable directly so Nix applies the input
    # override. Build precisely that derivation without re-reading the checkout.
    helper=${lib.escapeShellArg (builtins.readFile ../lib/deployment-build.nix)}
    drv=$(nix --extra-experimental-features 'nix-command flakes' eval \
      --raw --no-update-lock-file --no-write-lock-file --option pure-eval true \
      "''${override[@]}" "$ref#nixosConfigurations.$host" \
      --apply "host: (($helper) { rev = \"$rev\"; dirty = $dirty; tags = [ $tags ]; } host).config.system.build.$target.drvPath")
    if [[ $dirty == true ]] && {
      [[ $(git -C "$checkout" rev-parse --verify 'HEAD^{commit}') != "$rev" ]] ||
      [[ $(git -C "$checkout" status --porcelain --untracked-files=no) != "$status" ]]
    }; then
      echo "boreal-build: the downstream checkout changed during evaluation; retry" >&2
      exit 2
    fi
    exec nix --extra-experimental-features 'nix-command flakes' build \
      "$drv^out"
  '';
}
