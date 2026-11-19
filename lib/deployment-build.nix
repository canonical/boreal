release: host:
let
  source = host._module.specialArgs.self.sourceInfo or { };
  dirtyRev = source.dirtyRev or null;
in
if
  (source.rev or release.rev) != release.rev
  || (
    dirtyRev != null
    && (
      !release.dirty
      || !builtins.elem dirtyRev [
        release.rev
        (release.rev + "-dirty")
      ]
    )
  )
then
  throw "boreal-build: evaluated a different downstream revision; retry"
else if host ? extendModules && host ? options.boreal.deploymentRelease then
  host.extendModules { modules = [ { boreal.deploymentRelease = release; } ]; }
else
  throw "boreal-build: downstream configuration does not support deployment labels"
