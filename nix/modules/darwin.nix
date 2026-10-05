{ config, lib, ... }:
let
  cfg = config.programs.ra;
  serviceCfg = cfg.service;

  serviceName = "org.ra.serve";
in
{
  config = lib.mkIf cfg.enable {

    system.activationScripts.ra-datadir = lib.mkIf serviceCfg.enable ''
      mkdir -p ${lib.escapeShellArg serviceCfg.dataDir}
      chmod 770 ${lib.escapeShellArg serviceCfg.dataDir}
      chown root:wheel ${lib.escapeShellArg serviceCfg.dataDir}
    '';

    launchd.daemons.${serviceName} = lib.mkIf serviceCfg.enable {
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
        Label = serviceName;
        KeepAlive = true;
        RunAtLoad = true;
        WorkingDirectory = serviceCfg.dataDir;
        StandardOutPath = "/var/log/ra.out.log";
        StandardErrorPath = "/var/log/ra.err.log";
      };
    };

  };
}
