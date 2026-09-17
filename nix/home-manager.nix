self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.fut;
  tomlFormat = pkgs.formats.toml { };
in
{
  options.programs.fut = {
    enable = lib.mkEnableOption "fut, an agent-aware terminal multiplexer";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "fut.packages.<system>.default";
      description = "The fut package to install.";
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = { };
      description = ''
        Settings written to `$XDG_CONFIG_HOME/fut/config.toml`. Fut rejects
        unknown top-level keys, so only set keys the installed version
        understands. Leaving this empty writes no config file.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."fut/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = tomlFormat.generate "fut-config.toml" cfg.settings;
    };
  };
}
