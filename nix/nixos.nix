{
  pkgs,
  lib,
  ...
}: let
  beer = pkgs.callPackage ./package.nix {};
  remove = "${pkgs.coreutils}/bin/rm %t/beer-wayland-1.sock";
  target = ["graphical-session.target"];
in {
  environment.systemPackages = [beer];

  systemd.user.services.beer-server = {
    wantedBy = target;
    after = target;
    partOf = target;
    serviceConfig = {
      Type = "simple";
      ExecPreStart = remove;
      ExecStart = "${lib.getExe beer} --server";
      ExecStopPost = remove;
      Restart = "on-failure";
    };
  };
}
