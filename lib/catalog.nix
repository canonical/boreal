{ lib }:
let
  normalize = grant: if builtins.isList grant then lib.unique grant else grant;
in
{
  userPolicy =
    user: userCfg:
    let
      defaults = {
        read = userCfg.permissions.read or false;
        write = userCfg.permissions.write or false;
      };
    in
    {
      inherit user;
      permissions = lib.mapAttrs (_: normalize) defaults;
      workers = lib.mapAttrs (
        _: worker:
        let
          grants = worker.permissions or { };
          resolve =
            operation: default:
            let
              grant = grants.${operation} or null;
            in
            normalize (if grant == null then default else grant);
        in
        {
          package = worker.package.outputName;
          permissions = lib.mapAttrs resolve defaults;
        }
      ) (userCfg.workers or { });
    };
}
