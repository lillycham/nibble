# A home-manager module. It works the same under nix-darwin, NixOS and
# standalone home-manager, which a system module would not.
self:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.nibble;
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};
  json = pkgs.formats.json { };

  settings =
    lib.optionalAttrs (cfg.model != null) { inherit (cfg) model; }
    # A full path, because a launchd agent has almost nothing on its PATH.
    // lib.optionalAttrs (cfg.serverPackage != null) { server_command = lib.getExe cfg.serverPackage; }
    // cfg.settings;
  configFile = json.generate "nibble-config.json" settings;
in
{
  options.services.nibble = {
    enable = lib.mkEnableOption "nibble, a small local-model harness";

    package = lib.mkOption {
      type = lib.types.package;
      default = packages.nibble;
      defaultText = lib.literalExpression "nibble.packages.\${system}.nibble";
      description = "The nibble package.";
    };

    serverPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = packages.mlx-server or null;
      defaultText = lib.literalExpression "nibble.packages.\${system}.mlx-server, where it exists";
      description = ''
        The model server that `nibble serve` starts on demand. The default is
        mlx_lm.server, which only exists on Apple silicon. Null on other
        systems, where `settings.server_command` must name a server.
      '';
    };

    model = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/Users/me/.local/share/nibble/models/Qwen3-4B-Instruct-2507-4bit";
      description = ''
        Directory of the model to serve. Models stay outside the Nix store:
        they are large, and you will want to swap them without a rebuild.
      '';
    };

    serve.enable = lib.mkOption {
      type = lib.types.bool;
      default = cfg.model != null && (cfg.serverPackage != null || cfg.settings ? server_command);
      defaultText = lib.literalExpression "true when a model and a server are set";
      description = ''
        Run `nibble serve` in the background. It holds no model until
        something connects, and lets go of it again after `idle_seconds`.
      '';
    };

    gui.enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Install nibble-gui, the native chat window. macOS only for now.";
    };

    settings = lib.mkOption {
      type = json.type;
      default = { };
      example = { idle_seconds = 300; map_max_files = 40; roots = [ "~/devel" ]; };
      description = ''
        Written to nibble's config file. `nibble config` lists every setting
        and its value.
      '';
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      home.packages = [ cfg.package ];
      xdg.configFile."nibble/config.json".source = configFile;
    }

    (lib.mkIf cfg.gui.enable {
      home.packages = [ packages.nibble-gui ];
    })

    (lib.mkIf (cfg.serve.enable && pkgs.stdenv.hostPlatform.isDarwin) {
      launchd.agents.nibble = {
        enable = true;
        config = {
          ProgramArguments = [ (lib.getExe cfg.package) "serve" ];
          # Point at the store copy, so the agent restarts when a setting changes.
          EnvironmentVariables.NIBBLE_CONFIG = "${configFile}";
          RunAtLoad = true;
          KeepAlive = true;
          ProcessType = "Background";
          StandardOutPath = "${config.home.homeDirectory}/Library/Logs/nibble.log";
          StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/nibble.log";
        };
      };
    })

    (lib.mkIf (cfg.serve.enable && pkgs.stdenv.hostPlatform.isLinux) {
      systemd.user.services.nibble = {
        Unit.Description = "nibble model proxy";
        Service = {
          ExecStart = "${lib.getExe cfg.package} serve";
          Environment = "NIBBLE_CONFIG=${configFile}";
          Restart = "on-failure";
        };
        Install.WantedBy = [ "default.target" ];
      };
    })
  ]);
}
