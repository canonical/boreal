{
  ref,
  rev,
  host,
  tags,
}:

let
  flake = builtins.getFlake ref;
  base = flake.nixosConfigurations.${host};
  release = {
    inherit rev;
    tags = builtins.filter (tag: builtins.isString tag && tag != "") (builtins.split "\n" tags);
  };
  build =
    # Nix can omit the revision when the effective lock differs from the committed one.
    assert (flake.sourceInfo.rev or null) == rev;
    (
      if release.tags != [ ] && base ? extendModules && base ? options.boreal.deploymentRelease then
        base.extendModules { modules = [ { boreal.deploymentRelease = release; } ]; }
      else
        base
    ).config.system.build;
in
{
  system = build.toplevel;
  vm = build.vm;
}
