{
  config,
  pkgs,
  lib,
  ...
}: let
  cfg = config.programs.beer;
  remove = "${pkgs.coreutils}/bin/rm %t/beer-wayland-1.sock";
  target = ["graphical-session.target"];
in {
  options = {
    programs.beer = {
      enable = lib.mkEnableOption "";
      package = lib.mkOption {
        type = lib.types.package;
        default = pkgs.callPackage ./package.nix {};
        description = "Beer terminal package.";
      };
      server = {
        enable = lib.mkEnableOption "Enable starting the beer server via a systemd user service.";
        config = lib.mkOption {
          type = lib.types.str;
          default = "%h/.config/beer/beer.toml";
          description = "Path that the server will use for the beer config, as systemd user units do not automatically get environment variables. Uses systemd variables such as %h, %t, etc.";
        };
      };
    };
  };
  config = {
    environment.systemPackages = [cfg.package];

    systemd.user.services.beer-server = {
      inherit (cfg.server) enable;
      wantedBy = target;
      after = target;
      partOf = target;
      serviceConfig = {
        Type = "simple";
        ExecPreStart = remove;
        ExecStart = "${lib.getExe cfg.package} --server --config ${cfg.server.config}";
        ExecStopPost = remove;
        Restart = "on-failure";
      };
    };
  };
}
