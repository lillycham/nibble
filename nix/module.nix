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
    // lib.optionalAttrs (cfg.recipes != { }) { recipes = lib.mapAttrs (_: recipeSettings) cfg.recipes; }
    // cfg.settings;

  # Unset options stay out of the file, so the defaults apply. A recipe set
  # to null removes the built-in one of that name.
  recipeSettings = recipe: if recipe == null then null else lib.filterAttrs (_: value: value != null) recipe;

  recipe = lib.types.submodule {
    options = {
      description = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "One line for the list of commands.";
      };
      prompt = lib.mkOption {
        type = lib.types.str;
        description = ''
          What is sent. The text given with the command goes where `{input}`
          is, or after the prompt when there is no `{input}`.
        '';
      };
      system = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "A system prompt to use in place of the usual one.";
      };
      max_tokens = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.positive;
        default = null;
        description = "The reply length limit for this recipe.";
      };
      tools = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "Set to false to keep the model from reading files.";
      };
      quote = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "Have the model quote the lines its answer rests on, and check them.";
      };
      command = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf lib.types.str);
        default = null;
        example = [ "git" "diff" "--staged" ];
        description = ''
          A program and its arguments, run in the current directory when the
          recipe is given no text on the command line: its output is the input.
        '';
      };
    };
  };
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

    recipes = lib.mkOption {
      type = lib.types.attrsOf (lib.types.nullOr recipe);
      default = { };
      example = lib.literalExpression ''
        {
          review = {
            description = "Look over a diff for mistakes";
            prompt = "List the bugs in this diff, most serious first:\n{input}";
            command = [ "git" "diff" ];
            tools = false;
          };
          commit = null; # remove a built-in one
        }
      '';
      description = ''
        Named prompts with settings of their own. Each is `/NAME` in the chat
        and the window, and `nibble NAME` on the command line. `summarise` and
        `commit` are built in.
      '';
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
