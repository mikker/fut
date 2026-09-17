self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.fut;
in
{
  options.programs.fut = {
    enable = lib.mkEnableOption "fut, an agent-aware terminal multiplexer";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "fut.packages.<system>.default";
      description = "The fut package to install system wide.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
  };
}
