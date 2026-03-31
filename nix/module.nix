{ config, lib, pkgs, ... }:

let
  cfg = config.services.zeroclaw;
  settingsFormat = pkgs.formats.toml { };

  # Merge freeform settings with channel configs to produce base config.toml
  channelSettings = lib.filterAttrs (_: v: v != null) {
    channels_config = let
      channelConfigs = lib.filterAttrs (_: ch: ch.enable) cfg.channels;
      mkChannelConfig = _name: ch:
        lib.filterAttrs (_: v: v != null) (removeAttrs ch [
          "enable" "passwordFile" "botTokenFile"
        ]);
    in
      if channelConfigs == {} then null
      else lib.mapAttrs mkChannelConfig channelConfigs;
  };

  mergedSettings = lib.recursiveUpdate cfg.settings channelSettings;
  configFile = settingsFormat.generate "zeroclaw-config.toml" mergedSettings;

  stateDir = "/var/lib/${cfg.stateDirectory}";
  zeroclawDir = "${stateDir}/.zeroclaw";

  # Build secret injection script for preStart
  secretInjections = lib.concatStringsSep "\n" (lib.flatten [
    (lib.optional (cfg.channels.telegram.enable && cfg.channels.telegram.botTokenFile != null) ''
      token=$(cat ${lib.escapeShellArg cfg.channels.telegram.botTokenFile})
      ${pkgs.gnused}/bin/sed -i "s|__TELEGRAM_BOT_TOKEN__|$token|g" ${zeroclawDir}/config.toml
    '')
    (lib.optional (cfg.channels.email.enable && cfg.channels.email.passwordFile != null) ''
      pass=$(cat ${lib.escapeShellArg cfg.channels.email.passwordFile})
      ${pkgs.gnused}/bin/sed -i "s|__EMAIL_PASSWORD__|$pass|g" ${zeroclawDir}/config.toml
    '')
    (lib.optional (cfg.channels.xmpp.enable && cfg.channels.xmpp.passwordFile != null) ''
      pass=$(cat ${lib.escapeShellArg cfg.channels.xmpp.passwordFile})
      ${pkgs.gnused}/bin/sed -i "s|__XMPP_PASSWORD__|$pass|g" ${zeroclawDir}/config.toml
    '')
  ]);

  # Produce channel settings with placeholders for secrets
  telegramDefaults = lib.optionalAttrs (cfg.channels.telegram.enable && cfg.channels.telegram.botTokenFile != null) {
    channels_config.telegram.bot_token = "__TELEGRAM_BOT_TOKEN__";
  };
  emailDefaults = lib.optionalAttrs (cfg.channels.email.enable && cfg.channels.email.passwordFile != null) {
    channels_config.email.password = "__EMAIL_PASSWORD__";
  };
  xmppDefaults = lib.optionalAttrs (cfg.channels.xmpp.enable && cfg.channels.xmpp.passwordFile != null) {
    channels_config.xmpp.password = "__XMPP_PASSWORD__";
  };

  configWithPlaceholders = settingsFormat.generate "zeroclaw-config.toml"
    (lib.foldl lib.recursiveUpdate mergedSettings [
      telegramDefaults
      emailDefaults
      xmppDefaults
    ]);

  channelSubmodule = lib.types.submodule {
    options = {
      enable = lib.mkEnableOption "this channel";
    };
  };

  telegramSubmodule = lib.types.submodule {
    options = {
      enable = lib.mkEnableOption "Telegram channel";
      botTokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Path to file containing Telegram bot token";
      };
      allowed_users = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        description = "Telegram user IDs allowed to message the bot";
      };
    };
  };

  emailSubmodule = lib.types.submodule {
    options = {
      enable = lib.mkEnableOption "Email channel";
      passwordFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Path to file containing email account password";
      };
      imap_host = lib.mkOption {
        type = lib.types.str;
        description = "IMAP server hostname";
      };
      smtp_host = lib.mkOption {
        type = lib.types.str;
        description = "SMTP server hostname";
      };
      username = lib.mkOption {
        type = lib.types.str;
        description = "Email account username";
      };
      from_address = lib.mkOption {
        type = lib.types.str;
        description = "From address for outgoing email";
      };
      allowed_senders = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "*" ];
        description = "Allowed sender addresses (or * for all)";
      };
    };
  };

  xmppSubmodule = lib.types.submodule {
    options = {
      enable = lib.mkEnableOption "XMPP channel";
      passwordFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Path to file containing XMPP account password";
      };
      jid = lib.mkOption {
        type = lib.types.str;
        description = "Full JID for the bot (e.g. sid@example.com)";
      };
      server = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Server hostname (defaults to JID domain)";
      };
      port = lib.mkOption {
        type = lib.types.port;
        default = 5222;
        description = "XMPP server port";
      };
      ssl_verify = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Verify TLS certificates";
      };
      muc_rooms = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        description = "MUC room JIDs to auto-join";
      };
      muc_nick = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Nick to use in MUC rooms";
      };
    };
  };
in
{
  options.services.zeroclaw = {
    enable = lib.mkEnableOption "ZeroClaw AI agent service";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The ZeroClaw package to use";
    };

    settings = lib.mkOption {
      type = settingsFormat.type;
      default = {};
      description = ''
        Freeform ZeroClaw configuration. Rendered directly to config.toml.
        Any valid ZeroClaw config key can be set here.
      '';
    };

    channels = {
      telegram = lib.mkOption {
        type = telegramSubmodule;
        default = {};
        description = "Telegram channel configuration";
      };
      email = lib.mkOption {
        type = emailSubmodule;
        default = {};
        description = "Email channel configuration";
      };
      xmpp = lib.mkOption {
        type = xmppSubmodule;
        default = {};
        description = "XMPP channel configuration";
      };
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "zeroclaw";
      description = "System user for the ZeroClaw service";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "zeroclaw";
      description = "System group for the ZeroClaw service";
    };

    stateDirectory = lib.mkOption {
      type = lib.types.str;
      default = "zeroclaw";
      description = "State directory name under /var/lib/";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 18789;
      description = "ZeroClaw gateway port";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open firewall port for the ZeroClaw gateway";
    };

    environmentFiles = lib.mkOption {
      type = lib.types.listOf lib.types.path;
      default = [];
      description = ''
        List of environment files to load into the service.
        Compatible with agenix, sops-nix, or plain files.
      '';
    };

    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [];
      description = "Extra packages to add to the ZeroClaw service PATH";
    };

    pwaOverlay = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Path to PWA overlay directory for web frontend customization";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      home = stateDir;
      createHome = true;
      shell = "/sbin/nologin";
      description = "ZeroClaw AI agent service user";
    };

    users.groups.${cfg.group} = {};

    environment.systemPackages = [ cfg.package ];

    systemd.services.zeroclaw = {
      description = "ZeroClaw AI agent";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      path = (with pkgs; [
        bash
        coreutils
        findutils
        git
        gnugrep
        gnused
        gawk
        jq
        curl
        procps
        systemd
        util-linux
      ]) ++ cfg.extraPackages;

      preStart = ''
        # Copy base config (no secrets) from nix store to writable state dir
        mkdir -p ${zeroclawDir}
        rm -f ${zeroclawDir}/config.toml
        cp --no-preserve=mode ${configWithPlaceholders} ${zeroclawDir}/config.toml
        chmod 0600 ${zeroclawDir}/config.toml

        # Inject secrets from *File options
        ${secretInjections}
      '';

      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        WorkingDirectory = stateDir;
        Restart = "on-failure";
        RestartSec = "10s";
        TimeoutStopSec = "30s";

        ExecStartPre = "${pkgs.coreutils}/bin/sleep 3";
        ExecStart = "${cfg.package}/bin/zeroclaw daemon";

        Environment = [
          "HOME=${stateDir}"
          "SHELL=${pkgs.bash}/bin/bash"
          "ZEROCLAW_GATEWAY_TIMEOUT_SECS=120"
        ];
        EnvironmentFile = cfg.environmentFiles;

        # Systemd hardening
        ProtectHome = "tmpfs";
        ProtectSystem = "strict";
        PrivateTmp = true;
        PrivateDevices = true;
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectHostname = true;
        ProtectClock = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RemoveIPC = true;
        LockPersonality = true;
        ReadWritePaths = [ stateDir ];
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
        IPAddressDeny = "multicast";
        SystemCallFilter = [ "@system-service" ];
        SystemCallArchitectures = "native";
        RestrictNamespaces = true;
        UMask = "0027";
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];
  };
}
