let
  label = import ../lib/deployment-label.nix;
  rev = "d9c49750000000000000000000000000000000000";
  other = "248d380000000000000000000000000000000000";
  release = {
    inherit rev;
    tags = [
      "not-a-version"
      "v2.0.0"
      "v1.2.3-rc.1+build.2"
    ];
  };
in
assert label { source = { }; } == "unknown";
assert label { source = { inherit rev; }; } == "d9c4975";
assert label { source.shortRev = "d9c4975"; } == "d9c4975";
assert label { source.dirtyRev = rev; } == "d9c4975-dirty";
assert
  label {
    source.dirtyShortRev = "d9c4975";
    inherit release;
  } == "d9c4975-dirty";
assert
  label {
    source.rev = other;
    inherit release;
  } == "248d380";
assert
  label {
    source = { inherit rev; };
    release = release // {
      tags = [
        "non-version"
        "v2.0"
        "v1.2.3/extra"
      ];
    };
  } == "d9c4975";
assert
  label {
    source = { inherit rev; };
    inherit release;
  } == "v1.2.3-rc.1+build.2";
true
