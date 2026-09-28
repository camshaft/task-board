# NixOS module for the task-board service. capmesh-style: a dotfiles role does
#   imports = [ task-board.nixosModules.task-board ];
#   services.task-board.enable = true;
# and the daemon runs as a hardened systemd service, serving MCP (/mcp), the REST API
# (/api), and the web UI (/) on one port.
#
# Same no-auth-yet posture as the other LAN services: trust-on-first-use, sits behind
# the gateway. Add real auth (and gate the firewall port) when that lands.
self:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.task-board;
  pkg = self.packages.${pkgs.system}.task-board;

  # The daemon reads a TOML config file (--config); generate it from the options below.
  settings = {
    db_path = cfg.dbPath;
    host = cfg.host;
    port = cfg.port;
    webhook_timeout_secs = cfg.webhookTimeout;
  };
  configFile = (pkgs.formats.toml { }).generate "task-board.toml" settings;
in
{
  options.services.task-board = {
    enable = lib.mkEnableOption "the task-board agent coordination service";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkg;
      defaultText = lib.literalMD "the flake's `task-board` package";
      description = "The task-board package to run.";
    };

    host = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0";
      description = "Address to bind.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 8079;
      description = "Port to listen on for MCP, REST, and the UI.";
    };

    dbPath = lib.mkOption {
      type = lib.types.path;
      default = "/data/task-board/board.db";
      description = "SQLite database path. Kept off the root fs, on /data.";
    };

    webhookTimeout = lib.mkOption {
      type = lib.types.number;
      default = 5;
      description = "Best-effort webhook POST timeout in seconds.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Open the listen port in the firewall (LAN-only posture).";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "task-board";
      description = "User the service runs as.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "task-board";
      description = "Group the service runs as.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      description = "task-board service user";
    };
    users.groups.${cfg.group} = { };

    systemd.services.task-board = {
      description = "task-board: agent coordination board (MCP + REST + UI)";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      environment = {
        RUST_LOG = lib.mkDefault "info,task_board=debug";
      };

      serviceConfig = {
        # Create/own the DB directory as root before dropping privileges (the /data role
        # provides the mount; this just makes the subdir). The `+` runs it as root.
        ExecStartPre = "+${pkgs.coreutils}/bin/install -d -o ${cfg.user} -g ${cfg.group} -m 0750 ${builtins.dirOf cfg.dbPath}";
        ExecStart = "${lib.getExe cfg.package} --config ${configFile}";
        User = cfg.user;
        Group = cfg.group;
        Restart = "on-failure";
        RestartSec = 2;

        # Let systemd create/own the DB's parent dir under /data when it's a subdir of
        # a StateDirectory-style location; otherwise ensure it exists via a pre-start.
        StateDirectory = "task-board";

        # Hardening — this is stateless plumbing that only needs its DB dir writable.
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        # Grant write access to the DB's directory (e.g. /data/task-board).
        ReadWritePaths = [ (builtins.dirOf cfg.dbPath) ];
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];
  };
}
