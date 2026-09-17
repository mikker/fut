{
  description = "fut: an agent-aware terminal multiplexer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      home-manager,
    }:
    let
      lib = nixpkgs.lib;
      # x86_64-darwin was dropped from nixpkgs itself (2026-06-21).
      systems = [
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = lib.genAttrs systems;
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ self.overlays.default ];
        };

      nixosModule = import ./nix/module.nix self;
      homeManagerModule = import ./nix/home-manager.nix self;

      # environment.systemPackages exists identically on NixOS and nix-darwin,
      # so nix/module.nix works unmodified as both module outputs; this stub
      # lets the module evaluate without pulling in nix-darwin as a flake
      # input just to run the checks below.
      environmentSystemPackagesStub = {
        options.environment.systemPackages = lib.mkOption {
          type = lib.types.listOf lib.types.package;
          default = [ ];
        };
      };
    in
    {
      overlays.default = final: _prev: {
        fut = final.callPackage ./nix/package.nix { src = self; };
      };

      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.fut;
          fut = pkgs.fut;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ pkgs.fut ];
            packages = [
              pkgs.rust-analyzer
            ];
          };
        }
      );

      nixosModules.default = nixosModule;
      darwinModules.default = nixosModule;
      homeManagerModules.default = homeManagerModule;

      checks = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;

          homeManagerCheck =
            (home-manager.lib.homeManagerConfiguration {
              inherit pkgs;
              modules = [
                homeManagerModule
                {
                  home.username = "fut-check";
                  home.homeDirectory = "/home/fut-check";
                  home.stateVersion = "24.05";
                  programs.fut.enable = true;
                }
              ];
            }).activationPackage;

          moduleCheck =
            (lib.evalModules {
              modules = [
                environmentSystemPackagesStub
                nixosModule
                { programs.fut.enable = true; }
              ];
              specialArgs = {
                inherit pkgs;
              };
            }).config.environment.systemPackages;
        in
        {
          fut = pkgs.fut;
          home-manager-module = pkgs.runCommand "fut-home-manager-module-check" { } ''
            test -e ${homeManagerCheck}
            touch $out
          '';
          system-module = pkgs.runCommand "fut-system-module-check" { } ''
            test ${toString (builtins.length moduleCheck)} = "1"
            touch $out
          '';
        }
      );
    };
}
