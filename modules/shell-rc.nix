{ lib, pkgs }:
user: home: editMode:
let
  name = "boreal-user-rc-${user}";
  yamlFormat = pkgs.formats.yaml { };
  source = yamlFormat.generate "${name}.yaml" { edit_mode = editMode; };
in
{
  description = "Provision ~/.borealrc for Boreal user ${user}";
  wantedBy = [ "multi-user.target" ];
  before = [ "sshd.service" ];
  after = [ "local-fs.target" ];
  restartTriggers = [ source ];
  serviceConfig = {
    Type = "oneshot";
    RemainAfterExit = true;
    User = user;
    UMask = "0077";
    NoNewPrivileges = true;
    PrivateTmp = true;
    PrivateNetwork = true;
    ProtectSystem = "strict";
    ProtectProc = "invisible";
    ReadWritePaths = [ home ];
    ExecStart = pkgs.writeShellScript name ''
      set -eu
      home=${lib.escapeShellArg home}
      tmp="$home/.borealrc.tmp.$$"
      ${pkgs.coreutils}/bin/rm -f -- "$tmp"
      ${pkgs.coreutils}/bin/install -m 0600 ${source} "$tmp"
      ${pkgs.coreutils}/bin/mv -fT -- "$tmp" "$home/.borealrc"
    '';
  };
}
