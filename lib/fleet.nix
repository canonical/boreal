{ lib }:

let
  placements =
    config:
    lib.mapAttrs (_: user: lib.mapAttrs (_: worker: worker.placement) user.workers) config.boreal.users;
in
hosts:
let
  configs = lib.mapAttrs (_: host: host.config) hosts;
  central = lib.filterAttrs (_: host: host.boreal.role == "central") configs;
  centralNames = builtins.attrNames central;
  shared = placements central.${builtins.head centralNames};

  hostErrors = lib.concatLists (
    lib.mapAttrsToList (
      name: host:
      lib.optional (
        name != host.networking.hostName
      ) "Fleet key '${name}' must equal networking.hostName ('${host.networking.hostName}')."
      ++ lib.optional (!host.boreal.enable) "Boreal is disabled on fleet host '${name}'."
      ++ lib.optional (
        builtins.length centralNames == 1 && placements host != shared
      ) "Host '${name}' does not share the central worker placements."
      ++ map (entry: "Host '${name}': ${entry.message}") (
        lib.filter (entry: !entry.assertion) (host.assertions or [ ])
      )
    ) configs
  );

  workerErrors = lib.concatLists (
    lib.mapAttrsToList (
      owner: workers:
      lib.concatLists (
        lib.mapAttrsToList (
          worker: place:
          let
            name = "${owner}-${worker}";
            invalid = lib.filter (
              host: !(builtins.hasAttr host configs) || configs.${host}.boreal.role != "peripheral"
            ) place.peripherals;
          in
          lib.optional (!place.central && place.peripherals == [ ]) "Worker '${name}' has no placement."
          ++ lib.optional (
            lib.unique place.peripherals != place.peripherals
          ) "Worker '${name}' lists a peripheral more than once."
          ++ map (
            host: "Worker '${name}' targets '${host}', which is not a peripheral in this fleet."
          ) invalid
        ) workers
      )
    ) shared
  );
in
lib.optional (builtins.length centralNames != 1)
  "Expected exactly one central host, got ${toString (builtins.length centralNames)} (${lib.concatStringsSep ", " centralNames})."
++ hostErrors
++ lib.optionals (builtins.length centralNames == 1) workerErrors
