{ config, lib, ... }:
let
  cfg = config.programs.ra;
  serviceCfg = cfg.service;
in
{
  config = lib.mkIf cfg.enable {

    systemd.tmpfiles.rules = lib.mkIf serviceCfg.enable [
      "d ${serviceCfg.dataDir} 0770 root root -"
    ];

    systemd.services.ra-serve = lib.mkIf serviceCfg.enable {
      script = ''
        exec ${cfg.finalPackage}/bin/ra serve \
          --port ${toString serviceCfg.port} \
          --host ${lib.escapeShellArg serviceCfg.host} \
          --data-dir ${lib.escapeShellArg serviceCfg.dataDir} \
          --auth-token ${lib.escapeShellArg serviceCfg.authToken}
      '';

      environment = {
        ra_DATA_DIR = serviceCfg.dataDir;
        ra_AUTH_TOKEN = serviceCfg.authToken;
      };

      serviceConfig = {
        RestartSec = 5;
        Restart = "on-failure";
        WorkingDirectory = serviceCfg.dataDir;
        StandardOutput = "journal";
        StandardError = "journal";
      };

      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      description = "ra serve";
    };

  };
}
