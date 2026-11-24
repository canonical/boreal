{ lib }:

let
  keyFile = builtins.toFile "test-authorized-keys" ''
    # Test fixture, not real SSH keys.
    inline-key

      file-key
  '';
  check =
    keys: keyFiles: expected:
    let
      lab = import ../lab.nix {
        inherit lib;
        self = { };
        pkgs = { };
        config.users.users.pseudoc.openssh.authorizedKeys = { inherit keys keyFiles; };
      };
    in
    lab.boreal.admin.keys == expected && lab.boreal.users.sutton.keys == expected;
in
assert check [ ] [ ] [ ];
assert check [ ] [ keyFile ] [ "inline-key" "file-key" ];
assert check [ "inline-key" ] [ keyFile keyFile ] [ "inline-key" "file-key" ];
true
