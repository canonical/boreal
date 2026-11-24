{ self, pkgs, ... }:

{
  networking.hostName = "chopstick";
  time.timeZone = "Asia/Shanghai";

  boreal.role = "central";
}
