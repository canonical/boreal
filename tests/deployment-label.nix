let
  label = import ../lib/deployment-label.nix;
  applyRelease = import ../lib/deployment-build.nix;
  rev = "d9c4975000000000000000000000000000000000";
  other = "248d380000000000000000000000000000000000";
  release = {
    inherit rev;
    tags = [
      "not-a-version"
      "v2.0.0"
      "v1.2.3-rc.1+build.2"
    ];
  };
  host = source: {
    _module.specialArgs.self.sourceInfo = source;
    options.boreal.deploymentRelease = { };
    extendModules =
      { modules }:
      label {
        inherit source;
        release = (builtins.head modules).boreal.deploymentRelease;
      };
  };
  dirtyRelease = release // {
    dirty = true;
  };
in
assert label { source = { }; } == "unknown";
assert label { source = { inherit rev; }; } == "d9c4975";
assert label { source.shortRev = "d9c4975"; } == "d9c4975";
assert label { source.dirtyRev = rev; } == "d9c4975+";
assert
  label {
    source.dirtyShortRev = "d9c4975";
    inherit release;
  } == "d9c4975+";
assert
  label {
    source = { };
    release = {
      inherit rev;
      tags = [ ];
    };
  } == "d9c4975";
assert
  label {
    source = { };
    inherit release;
  } == "v1.2.3-rc.1+build.2";
assert
  label {
    source = { };
    release = release // {
      dirty = true;
    };
  } == "d9c4975+";
assert
  label {
    source = { };
    release = release // {
      rev = "bad";
    };
  } == "unknown";
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
assert applyRelease release (host { }) == "v1.2.3-rc.1+build.2";
assert
  applyRelease dirtyRelease (host {
    dirtyRev = "${rev}-dirty";
  }) == "d9c4975+";
assert
  !(builtins.tryEval (
    applyRelease release (host {
      rev = other;
    })
  )).success;
assert
  !(builtins.tryEval (
    applyRelease (release // { dirty = false; }) (host {
      dirtyRev = rev;
    })
  )).success;
true
