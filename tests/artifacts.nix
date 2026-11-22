{ pkgs }:

pkgs.runCommandCC "boreal-artifact-tests"
  {
    nativeBuildInputs = [
      pkgs.nix
      pkgs.rustc
    ];
  }
  ''
    rustc --edition=2024 --test ${../sdk/artifact.rs} -o artifact-unit
    ./artifact-unit
    cp ${./artifacts.rs} probe.rs
    cp ${../sdk/artifact.rs} artifact.rs
    rustc --edition=2024 probe.rs -o artifact-probe
    bash ${./artifacts.sh} "$PWD/artifact-probe"
    touch "$out"
  ''
