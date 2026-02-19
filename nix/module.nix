inputs:

{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.mangochill;
in
{
  options.services.mangochill = {
    enable = lib.mkEnableOption "mangochill FPS limiter";

    package = lib.mkOption {
      type = lib.types.package;
      default = inputs.self.packages.${pkgs.system}.mangochill;
      description = "The MangoChill package to use.";
    };

    mangohudPackage = lib.mkOption {
      type = lib.types.package;
      default = inputs.self.packages.${pkgs.system}.mangohud;
      description = "The MangoHud package to use (custom fork with control socket support).";
    };
  };

  config = lib.mkIf cfg.enable {
    services.udev.extraRules = ''
      SUBSYSTEM=="input", KERNEL=="event*", ACTION=="add|change", RUN+="${pkgs.acl}/bin/setfacl -m u:mangochill:r $devnode"
    '';

    users = {
      users.mangochill = {
        isSystemUser = true;
        group = "mangochill";
      };
      groups.mangochill = { };
    };

    programs.steam.extraPackages = [ cfg.mangohudPackage ];

    systemd.services.mangochill-server = {
      description = "Mangochill FPS limiter server";
      wantedBy = [ "multi-user.target" ];
      after = [ "systemd-udev-settle.service" ];

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/mangochill-server -vv";
        User = "mangochill";
        Group = "mangochill";
        Type = "simple";
        Restart = "on-failure";
        RestartSec = 5;

        RuntimeDirectory = "mangochill";
        RuntimeDirectoryMode = "0755";

        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;

        DeviceAllow = [ "char-input r" ];
      };
    };

    environment.systemPackages = [
      cfg.package
      cfg.mangohudPackage
    ];
  };
}
