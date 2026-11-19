{ lib }:

let
  validate = import ../lib/fleet.nix { inherit lib; };
  host = hostName: role: placement: {
    config = {
      networking = { inherit hostName; };
      boreal = {
        enable = true;
        inherit role;
        users.alice.workers.greet = { inherit placement; };
      };
    };
  };
  fleet = placement: {
    lab = host "lab" "central" placement;
    compute-01 = host "compute-01" "peripheral" placement;
  };
  valid = {
    central = true;
    peripherals = [ "compute-01" ];
  };
  hosts = fleet valid;
  error = text: example: lib.any (lib.hasInfix text) (validate example);
in
assert validate hosts == [ ];
assert validate (fleet (valid // { central = false; })) == [ ];
assert error "got 0" { inherit (hosts) compute-01; };
assert error "got 2" (hosts // { compute-01 = host "compute-01" "central" valid; });
assert error "not a peripheral" (fleet (valid // { peripherals = [ "lab" ]; }));
assert error "not a peripheral" (fleet (valid // { peripherals = [ "unknown" ]; }));
assert error "more than once" (
  fleet (
    valid
    // {
      peripherals = [
        "compute-01"
        "compute-01"
      ];
    }
  )
);
assert error "no placement" (fleet {
  central = false;
  peripherals = [ ];
});
assert error "does not share" (
  hosts // { compute-01 = host "compute-01" "peripheral" (valid // { central = false; }); }
);
assert error "must equal networking.hostName" (
  hosts // { compute-01 = host "other" "peripheral" valid; }
);
true
