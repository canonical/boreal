#!/usr/bin/env bash
set -euo pipefail

probe=$1
scratch=$(mktemp -d "${TMPDIR:-/tmp}/artifact-test.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
export HOME="$scratch/home"
export NIX_CONFIG=$'experimental-features = nix-command\nsubstituters =\nrequire-sigs = true'
mkdir -p "$HOME" "$scratch"/{cache,create,deploy-a,deploy-b}
printf 'synthetic ISO fixture\n' > "$scratch/image.iso"

export BOREAL_ARTIFACT_READ_STORE="file://$scratch/cache"
export BOREAL_ARTIFACT_WRITE_STORE="$BOREAL_ARTIFACT_READ_STORE?compression=zstd"
export BOREAL_ARTIFACT_RUNTIME_DIR="$scratch/create"
export NIX_REMOTE="$scratch/producer"

# The cache cannot acknowledge a publish to a directory that is a regular file.
if BOREAL_ARTIFACT_WRITE_STORE="file://$scratch/image.iso" \
  "$probe" publish "$scratch/image.iso" > "$scratch/failed-ref"; then
  echo 'publish unexpectedly succeeded' >&2
  exit 1
fi
test ! -s "$scratch/failed-ref"

artifact=$("$probe" publish "$scratch/image.iso")
cp "$scratch/image.iso" "$scratch/different-name.iso"
test "$("$probe" publish "$scratch/different-name.iso")" = "$artifact"
test -z "$(nix-store --query --roots "$artifact")"
unset BOREAL_ARTIFACT_WRITE_STORE

# Distinct local stores model different machines. Runtime dirs model services
# on the same machine, but they do not contain artifact bytes.
export NIX_REMOTE="$scratch/consumer"
export BOREAL_ARTIFACT_RUNTIME_DIR="$scratch/deploy-a"
test ! -e "$NIX_REMOTE$artifact"
test "$("$probe" fetch "$artifact")" = "$artifact"
cmp "$scratch/image.iso" "$NIX_REMOTE$artifact"
test -z "$(nix-store --query --roots "$artifact")"

# An invalid endpoint makes any central lookup fail, even a metadata request.
export BOREAL_ARTIFACT_READ_STORE="file://$scratch/image.iso"
export BOREAL_ARTIFACT_RUNTIME_DIR="$scratch/deploy-b"
test "$("$probe" fetch "$artifact")" = "$artifact"
test -z "$(nix-store --query --roots "$artifact")"
